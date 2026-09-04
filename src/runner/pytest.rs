//! pytest-compatible runner.
//!
//! Invokes `<exe> -x --tb=no -q [--hypothesis-seed=N] [<extra args>]
//! <mirrored-tests>` per mutant inside the per-worker project mirror managed
//! by [`with_worker_mirror`](super::with_worker_mirror). Exit 0 → mutant
//! survived; non-zero → killed; wall-clock past `--timeout` → timed out.
//!
//! `exe` is the framework executable — `pytest`, or `rstest` (a pytest-CLI-
//! compatible drop-in). Both accept the same flags and node-id positionals, so
//! one implementation drives either; only the program name differs.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use wait_timeout::ChildExt;

use super::process_group::{kill_group, with_new_process_group};
use super::{
    apply_patch, build_mirror, mirror_pythonpath, run_baseline_with_timeout, with_worker_mirror,
    BaselineStatus, Runner,
};
use crate::config::IsolationMode;
use crate::filter::coverage::CoverageContexts;
use crate::mutator::Mutant;
use crate::report::MutantOutcome;

pub struct PytestRunner {
    tests: PathBuf,
    timeout: Duration,
    baseline_timeout: Duration,
    hypothesis_seed: Option<u64>,
    extra_args: Vec<String>,
    isolation: IsolationMode,
    coverage: Option<Arc<CoverageContexts>>,
    /// Interpreter to run the framework with. `Some` → `<python> -m <exe>`;
    /// `None` → bare `<exe>` resolved from `PATH` (historical behavior).
    python: Option<PathBuf>,
    /// Framework executable / module name: `pytest` or the `rstest` drop-in.
    exe: &'static str,
    /// Smart test ordering: when on, coverage-selected tests are reordered so
    /// the most targeted one (fewest lines covered) runs first, so `-x`
    /// short-circuits sooner. Only permutes the set — never changes a verdict.
    smart_order: bool,
}

impl PytestRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tests: PathBuf,
        timeout: Duration,
        baseline_timeout: Duration,
        hypothesis_seed: Option<u64>,
        extra_args: Vec<String>,
        isolation: IsolationMode,
        coverage: Option<Arc<CoverageContexts>>,
        python: Option<PathBuf>,
        exe: &'static str,
        smart_order: bool,
    ) -> Self {
        Self {
            tests,
            timeout,
            baseline_timeout,
            hypothesis_seed,
            extra_args,
            isolation,
            coverage,
            python,
            exe,
            smart_order,
        }
    }

    /// Base command that launches the framework. With a configured interpreter
    /// we go through `<python> -m <exe>` so the run uses *that* interpreter's
    /// framework (and its venv's installed packages) with no reliance on `PATH`.
    /// Without one we keep spawning a bare `<exe>` console script from PATH.
    fn framework_command(&self) -> Command {
        match &self.python {
            Some(py) => {
                let mut cmd = Command::new(py);
                cmd.arg("-m").arg(self.exe);
                cmd
            }
            None => Command::new(self.exe),
        }
    }
}

impl Runner for PytestRunner {
    fn run(&self, mutant: &Mutant) -> Result<MutantOutcome> {
        with_worker_mirror(&self.tests, self.isolation, |mirror| {
            let _guard = apply_patch(mirror, mutant)?;

            let mut cmd = self.framework_command();
            cmd.arg("-x").arg("--tb=no").arg("-q");
            if let Some(seed) = self.hypothesis_seed {
                cmd.arg(format!("--hypothesis-seed={seed}"));
            }
            for arg in &self.extra_args {
                cmd.arg(arg);
            }
            // Coverage-driven selection: when contexts are available we know
            // exactly which tests cross the mutated line. Pass them as positional
            // node ids and skip the default "whole tests dir" sweep. The filter
            // chain has already dropped mutants with no recorded context, so
            // we only get here when at least one test id exists.
            //
            // Smart ordering (cold-start): reorder those ids so the most
            // *targeted* test — fewest lines covered — runs first, letting `-x`
            // short-circuit sooner. Only permutes the set, so the verdict is
            // unchanged.
            match self.coverage.as_ref() {
                Some(ctx) => match ctx.tests_for_mutant(mutant) {
                    Some(ids) if !ids.is_empty() => {
                        if self.smart_order {
                            for id in ctx.order_by_breadth(ids) {
                                cmd.arg(id);
                            }
                        } else {
                            for id in ids {
                                cmd.arg(id);
                            }
                        }
                    }
                    _ => {
                        cmd.arg(&mirror.tests);
                    }
                },
                None => {
                    cmd.arg(&mirror.tests);
                }
            }
            cmd.current_dir(&mirror.root);
            with_new_process_group(&mut cmd);
            // Editable installs (`pip install -e .`) write an absolute path
            // into a `.pth` file pointing at the ORIGINAL source tree. Without
            // PYTHONPATH, pytest in the mirror still resolves `import <pkg>`
            // to the original src and never sees the mutation. Prepend the
            // mirror's `src/` (and root) so the mirror's mutated package
            // wins over the editable install in sys.path.
            cmd.env("PYTHONPATH", mirror_pythonpath(mirror)?);
            // Never let pytest write `.pyc` into the reused mirror — a stale
            // compiled module that still validates against the patched source
            // would mask the mutation and falsely report it survived.
            cmd.env("PYTHONDONTWRITEBYTECODE", "1");
            // Discard per-mutant pytest output. Only the exit status matters
            // (0 → survived, non-zero → killed); without this each killed
            // mutant streams its `FAILED … / 1 failed` lines to the terminal,
            // which reads as the run being broken when it's mutants dying as
            // intended. The baseline run (separate) still captures + surfaces
            // output so a red unmutated suite is diagnosable.
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let mut child = cmd.spawn().context("spawning test runner")?;

            match child
                .wait_timeout(self.timeout)
                .context("waiting on test runner")?
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
        // Build a dedicated mirror (no patch applied) and run the FULL test
        // suite once. No `-x`: we want pytest's complete failure summary, not
        // just the first failure, so the abort message is diagnosable. The
        // wall-clock cap is `--baseline-timeout` (separate from the per-mutant
        // `--timeout`, which sizes a coverage-selected subset) so a hung suite
        // can't stall the whole run forever.
        let mirror = build_mirror(&self.tests, self.isolation)?;
        let mut cmd = self.framework_command();
        cmd.arg("--tb=line").arg("-q");
        if let Some(seed) = self.hypothesis_seed {
            cmd.arg(format!("--hypothesis-seed={seed}"));
        }
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        cmd.arg(&mirror.tests);
        cmd.current_dir(&mirror.root);
        with_new_process_group(&mut cmd);
        cmd.env("PYTHONPATH", mirror_pythonpath(&mirror)?);
        run_baseline_with_timeout(cmd, self.baseline_timeout)
    }
}
