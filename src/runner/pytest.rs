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

use std::io::Read;
use std::path::PathBuf;
use std::process::{ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
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
    /// Smart test ordering on. When set, coverage-selected ids are reordered by
    /// `kill_order` and killers learned into `kill_sink`.
    smart_order: bool,
    /// Immutable kill-order history, read to order selected tests (no lock).
    kill_order: Arc<crate::kill_order::KillOrder>,
    /// Kills learned this run, folded into the sidecar by the engine afterward.
    kill_sink: Arc<Mutex<Vec<crate::kill_order::KillRecord>>>,
    /// Base for the `(file, operator)` history key — mutant files are made
    /// relative to this so the sidecar survives moves/checkouts.
    key_base: PathBuf,
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
        kill_order: Arc<crate::kill_order::KillOrder>,
        kill_sink: Arc<Mutex<Vec<crate::kill_order::KillRecord>>>,
        key_base: PathBuf,
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
            kill_order,
            kill_sink,
            key_base,
        }
    }

    /// `(project-relative file, operator name)` history key for a mutant.
    fn kill_key(&self, mutant: &Mutant) -> (String, &'static str) {
        let file = mutant
            .file
            .strip_prefix(&self.key_base)
            .unwrap_or(&mutant.file)
            .to_string_lossy()
            .replace('\\', "/");
        (file, mutant.operator.name())
    }

    /// Read the killed mutant's captured pytest output, parse the killing test's
    /// node id from the `-rf` summary, and append it to the shared sink for the
    /// engine to fold into the kill-order sidecar. Best-effort: any misfire
    /// (no output, unparsable, poisoned lock) just forfeits this datapoint.
    fn record_killer(&self, stdout: Option<ChildStdout>, file: &str, op: &'static str) {
        let Some(mut out) = stdout else {
            return;
        };
        let mut buf = String::new();
        if out.read_to_string(&mut buf).is_err() {
            return;
        }
        if let Some(nodeid) = crate::kill_order::parse_first_failed(&buf) {
            if let Ok(mut sink) = self.kill_sink.lock() {
                sink.push(crate::kill_order::KillRecord {
                    file: file.to_string(),
                    operator: op.to_string(),
                    nodeid,
                });
            }
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
    fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
        self.kill_sink
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    fn run(&self, mutant: &Mutant) -> Result<MutantOutcome> {
        with_worker_mirror(&self.tests, self.isolation, |mirror| {
            let _guard = apply_patch(mirror, mutant)?;

            let mut cmd = self.framework_command();
            cmd.arg("-x").arg("--tb=no").arg("-q");
            // `-rf` prints a `FAILED <nodeid>` summary line so a kill tells us
            // *which* test did it — the signal smart ordering learns from.
            if self.smart_order {
                cmd.arg("-rf");
            }
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
            // Smart ordering reorders those ids so a historically-frequent
            // killer for this `(file, operator)` runs first — pytest's `-x` then
            // short-circuits on it. It only permutes the set, never changes it,
            // so the kill/survive verdict is identical.
            let (key_file, key_op) = self.kill_key(mutant);
            match self
                .coverage
                .as_ref()
                .and_then(|ctx| ctx.tests_for_mutant(mutant))
            {
                Some(ids) if !ids.is_empty() => {
                    if self.smart_order {
                        for id in self.kill_order.order(ids, &key_file, key_op) {
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
            // Per-mutant pytest output is noise on the terminal (each killed
            // mutant would stream `FAILED … / 1 failed`, reading as breakage
            // when it's mutants dying as intended). Discard it — except under
            // smart ordering, where we capture stdout to learn the killer's
            // node id from the `-rf` summary. Output is tiny (`-q --tb=no`), so
            // a pipe never deadlocks. stderr is always discarded.
            let stdout_cfg = if self.smart_order {
                Stdio::piped()
            } else {
                Stdio::null()
            };
            cmd.stdout(stdout_cfg).stderr(Stdio::null());
            let mut child = cmd.spawn().context("spawning test runner")?;
            let stdout = child.stdout.take();

            match child
                .wait_timeout(self.timeout)
                .context("waiting on test runner")?
            {
                Some(status) => {
                    if status.success() {
                        Ok(MutantOutcome::survived(mutant.clone()))
                    } else {
                        // Killed. Under smart ordering, learn which test did it
                        // and record it for next time's ordering.
                        if self.smart_order {
                            self.record_killer(stdout, &key_file, key_op);
                        }
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
