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
use std::process::{Child, Command, ExitStatus, Stdio};
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

    /// Parse the killing test's node id from captured pytest output (the `-rfE`
    /// summary) and append it to the shared sink for the engine to fold into the
    /// kill-order sidecar. Best-effort: any misfire (no failure line, unparsable,
    /// poisoned lock) just forfeits this datapoint.
    fn record_killer(&self, output: &str, file: &str, op: &'static str) {
        if let Some(nodeid) = crate::kill_order::parse_first_failed(output) {
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

/// Whether to capture the per-mutant pytest stdout to learn the killing test.
/// Only worth it when smart ordering is on AND more than one test was selected:
/// with a single (or zero) selected test there's nothing to reorder next run, so
/// the pipe + read + `-rfE` are pure overhead. Kept pure so the wiring is tested.
fn should_capture(smart_order: bool, selected_len: usize) -> bool {
    smart_order && selected_len > 1
}

/// Per-mutant test selection: either coverage-selected node ids or the
/// whole-tests-dir sweep. Returned by [`select_args`] so the caller keeps the
/// sweep path as an `OsStr` (no lossy conversion) while the id wiring stays pure
/// and testable.
enum Selection {
    /// Positional args to append after the base flags: the (reordered) node
    /// ids, plus `-rfE` when `capture` is set so the summary names the killer.
    Ids { args: Vec<String>, capture: bool },
    /// No coverage selection → run the whole tests dir. Never captures.
    Sweep,
}

/// Decide the per-mutant selection. Pure — no mirror, no spawn — so the arg
/// wiring (smart reordering, the `-rfE` capture flag, and the sweep fallback)
/// is unit-tested without launching pytest. `ids` is the coverage selection;
/// `None` or empty falls back to [`Selection::Sweep`].
fn select_args(
    ids: Option<&[String]>,
    smart_order: bool,
    kill_order: &crate::kill_order::KillOrder,
    key_file: &str,
    key_op: &str,
) -> Selection {
    match ids {
        Some(ids) if !ids.is_empty() => {
            let ordered = if smart_order {
                kill_order.order(ids, key_file, key_op)
            } else {
                ids.to_vec()
            };
            let capture = should_capture(smart_order, ordered.len());
            let mut args = ordered;
            if capture {
                // `-rfE` prints a `FAILED`/`ERROR <nodeid>` summary line so a
                // kill tells us *which* test did it — a mutant can die by a
                // failed assertion or a raised error, so accept either.
                args.push("-rfE".to_string());
            }
            Selection::Ids { args, capture }
        }
        _ => Selection::Sweep,
    }
}

/// Grace period to wait for the stdout drain once the child has exited. In the
/// normal case the drain finishes instantly (the child closing its write end
/// EOFs the read). It only matters when a lingering grandchild inherited the
/// pipe's write end: rather than block a worker forever we abandon the drain
/// after this and forfeit the (advisory) datapoint.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Wait for `child` with `timeout` while concurrently draining its stdout on a
/// dedicated thread. Draining concurrently is load-bearing: if the child fills
/// its stdout pipe (~64 KiB) it blocks on `write`, which would wedge
/// `wait_timeout` forever and turn a kill into a false timeout — silently
/// changing the verdict. On timeout, `on_timeout` terminates the child. Returns
/// the exit status (`None` = timed out) and the captured stdout (`None` when
/// stdout wasn't piped, or on the timeout path).
///
/// The drain result is delivered over a channel, not a bare `JoinHandle::join`,
/// so the wait for it is *bounded* by [`DRAIN_GRACE`]: if the child exits but a
/// grandchild still holds the pipe's write end the read never EOFs, and joining
/// would wedge this worker forever. Bytes are decoded lossily rather than via
/// `read_to_string` so a single non-UTF-8 byte can't discard the whole capture.
fn wait_draining_stdout(
    child: Child,
    timeout: Duration,
    on_timeout: impl FnOnce(&mut Child),
) -> Result<(Option<ExitStatus>, Option<String>)> {
    wait_draining_stdout_with_grace(child, timeout, DRAIN_GRACE, on_timeout)
}

/// [`wait_draining_stdout`] with the drain grace injected, so a test can prove
/// the bound holds without waiting the full [`DRAIN_GRACE`].
fn wait_draining_stdout_with_grace(
    mut child: Child,
    timeout: Duration,
    grace: Duration,
    on_timeout: impl FnOnce(&mut Child),
) -> Result<(Option<ExitStatus>, Option<String>)> {
    let reader = child.stdout.take().map(|mut out| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        rx
    });
    // `None` when stdout wasn't piped or the drain overran the grace window.
    let collect = |rx: Option<std::sync::mpsc::Receiver<String>>| {
        rx.and_then(|rx| rx.recv_timeout(grace).ok())
    };

    match child
        .wait_timeout(timeout)
        .context("waiting on test runner")?
    {
        Some(status) => Ok((Some(status), collect(reader))),
        None => {
            on_timeout(&mut child);
            let _ = child.wait();
            // Reap the drain now that the pipe has closed (bounded like above).
            let _ = collect(reader);
            Ok((None, None))
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
            // so the kill/survive verdict is identical. See [`select_args`].
            let (key_file, key_op) = self.kill_key(mutant);
            let ids = self
                .coverage
                .as_ref()
                .and_then(|ctx| ctx.tests_for_mutant(mutant));
            let capture =
                match select_args(ids, self.smart_order, &self.kill_order, &key_file, key_op) {
                    Selection::Ids { args, capture } => {
                        for a in &args {
                            cmd.arg(a);
                        }
                        capture
                    }
                    Selection::Sweep => {
                        cmd.arg(&mirror.tests);
                        false
                    }
                };
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
            // when it's mutants dying as intended). Discard it — except when
            // capturing to learn the killer's node id from the `-rfE` summary.
            // stderr is always discarded.
            let stdout_cfg = if capture {
                Stdio::piped()
            } else {
                Stdio::null()
            };
            cmd.stdout(stdout_cfg).stderr(Stdio::null());
            let child = cmd.spawn().context("spawning test runner")?;

            match wait_draining_stdout(child, self.timeout, kill_group)? {
                (Some(status), output) => {
                    if status.success() {
                        Ok(MutantOutcome::survived(mutant.clone()))
                    } else {
                        // Killed. When capturing, learn which test did it and
                        // record it for next time's ordering.
                        if let Some(out) = output {
                            self.record_killer(&out, &key_file, key_op);
                        }
                        Ok(MutantOutcome::killed(mutant.clone()))
                    }
                }
                (None, _) => Ok(MutantOutcome::timed_out(mutant.clone())),
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- should_capture: locks the #4 wiring (no capture unless it can pay off).

    #[test]
    fn should_capture_only_when_ordering_on_and_multiple_tests() {
        // Off → never capture, regardless of how many tests were selected.
        assert!(!should_capture(false, 0));
        assert!(!should_capture(false, 1));
        assert!(!should_capture(false, 5));
        // On but ≤1 selected → nothing to reorder next run, so no capture.
        assert!(!should_capture(true, 0));
        assert!(!should_capture(true, 1));
        // On and >1 selected → capture to learn the killer.
        assert!(should_capture(true, 2));
        assert!(should_capture(true, 100));
    }

    // --- select_args: locks the arg wiring (ordering, `-rfE`, sweep fallback).

    use crate::kill_order::KillOrder;

    fn sids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn assert_ids(sel: Selection, want_args: &[&str], want_capture: bool) -> Vec<String> {
        match sel {
            Selection::Ids { args, capture } => {
                assert_eq!(args, sids(want_args), "positional args");
                assert_eq!(capture, want_capture, "capture flag");
                args
            }
            Selection::Sweep => panic!("expected Ids, got Sweep"),
        }
    }

    #[test]
    fn select_args_multi_test_captures_and_appends_rfe() {
        // Smart order on + >1 test → capture, and `-rfE` must be the last arg
        // so the summary surfaces the killer. This is the wiring the ignored
        // e2e can't cheaply guard.
        let ko = KillOrder::default();
        let sel = select_args(Some(&sids(&["t::a", "t::b"])), true, &ko, "src/f.py", "op");
        assert_ids(sel, &["t::a", "t::b", "-rfE"], true);
    }

    #[test]
    fn select_args_single_test_neither_captures_nor_adds_rfe() {
        // One selected test → nothing to reorder next run, so no capture and no
        // `-rfE` (piping + summary would be pure overhead).
        let ko = KillOrder::default();
        let sel = select_args(Some(&sids(&["t::only"])), true, &ko, "src/f.py", "op");
        assert_ids(sel, &["t::only"], false);
    }

    #[test]
    fn select_args_smart_order_off_keeps_input_order_no_rfe() {
        // Ordering off → ids pass through untouched, never capture even with
        // many tests.
        let ko = KillOrder::default();
        let sel = select_args(
            Some(&sids(&["t::a", "t::b", "t::c"])),
            false,
            &ko,
            "src/f.py",
            "op",
        );
        assert_ids(sel, &["t::a", "t::b", "t::c"], false);
    }

    #[test]
    fn select_args_reorders_by_history_then_appends_rfe() {
        // A learned killer must be lifted ahead of the coverage order, and
        // `-rfE` still trails the reordered ids.
        let mut ko = KillOrder::default();
        ko.record("src/f.py", "op", "t::c");
        ko.record("src/f.py", "op", "t::c");
        let sel = select_args(
            Some(&sids(&["t::a", "t::b", "t::c"])),
            true,
            &ko,
            "src/f.py",
            "op",
        );
        assert_ids(sel, &["t::c", "t::a", "t::b", "-rfE"], true);
    }

    #[test]
    fn select_args_no_or_empty_coverage_is_sweep() {
        let ko = KillOrder::default();
        assert!(matches!(
            select_args(None, true, &ko, "src/f.py", "op"),
            Selection::Sweep
        ));
        assert!(matches!(
            select_args(Some(&[]), true, &ko, "src/f.py", "op"),
            Selection::Sweep
        ));
    }

    // --- wait_draining_stdout: guards the #2 deadlock fix.

    fn never_called(_: &mut Child) {
        panic!("on_timeout must not fire when the child exits before the timeout");
    }

    #[test]
    #[cfg(unix)]
    fn wait_draining_does_not_deadlock_on_large_output() {
        // The child writes ~264 KiB to stdout — far past the ~64 KiB pipe
        // buffer. Without concurrent draining the child would block on `write`,
        // wedging the wait forever (the exact bug that turned a kill into a
        // false timeout). With draining it must complete and capture all bytes.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("i=0; while [ $i -lt 8000 ]; do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\n'; i=$((i+1)); done")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn sh flooder");

        let (status, output) =
            wait_draining_stdout(child, Duration::from_secs(30), never_called).unwrap();
        assert!(status.expect("child exited, not timed out").success());
        let out = output.expect("stdout was piped");
        assert!(
            out.len() >= 200_000,
            "expected the full flood to be drained, got {} bytes",
            out.len()
        );
    }

    #[test]
    #[cfg(unix)]
    fn wait_draining_does_not_hang_when_grandchild_holds_stdout() {
        // Regression for the bounded-drain fix: the direct child exits, but a
        // backgrounded grandchild inherited the stdout write end, so the read
        // never EOFs. An unbounded `join` would wedge the worker forever; the
        // channel + grace must return promptly instead. `on_timeout` must NOT
        // fire — the child exited normally, it did not time out.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo done") // shell exits; the `sleep` keeps stdout open
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn sh");

        let grace = Duration::from_millis(300);
        let start = std::time::Instant::now();
        let (status, output) =
            wait_draining_stdout_with_grace(child, Duration::from_secs(30), grace, never_called)
                .unwrap();
        let elapsed = start.elapsed();

        assert!(
            status.expect("child exited, not timed out").success(),
            "the direct child exited 0"
        );
        assert!(
            output.is_none(),
            "drain can't EOF while the grandchild holds the pipe → no capture"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "must return within the grace, not block on the never-EOFing pipe (took {elapsed:?})"
        );
    }

    #[test]
    #[cfg(unix)]
    fn wait_draining_keeps_capture_despite_non_utf8_bytes() {
        // A stray non-UTF-8 byte must not discard the whole capture (the old
        // `read_to_string` behavior): we lossy-decode, so the surrounding
        // `FAILED <nodeid>` line still survives to be parsed. Prints a raw
        // 0xFF byte, then a normal summary line.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf '\\377\\n'; printf 'FAILED tests/t.py::x - E\\n'")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn sh");

        let (status, output) =
            wait_draining_stdout(child, Duration::from_secs(30), never_called).unwrap();
        assert!(status.expect("exited").success());
        let out = output.expect("stdout was piped");
        assert_eq!(
            crate::kill_order::parse_first_failed(&out).as_deref(),
            Some("tests/t.py::x"),
            "lossy decode must preserve the FAILED line, got {out:?}"
        );
    }

    #[test]
    fn wait_draining_returns_none_output_when_stdout_not_piped() {
        // stdout null → no reader thread, output is None, status still returned.
        let mut cmd = Command::new(if cfg!(windows) { "cmd" } else { "true" });
        if cfg!(windows) {
            cmd.arg("/C").arg("exit 0");
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn trivial process");

        let (status, output) =
            wait_draining_stdout(child, Duration::from_secs(30), never_called).unwrap();
        assert!(status.expect("exited").success());
        assert!(output.is_none(), "no pipe → no captured output");
    }

    #[test]
    #[cfg(unix)]
    fn wait_draining_times_out_and_invokes_on_timeout() {
        // A child that outlives the timeout must return `(None, None)` and have
        // `on_timeout` invoked exactly once to terminate it.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn sleeper");

        let mut killed = false;
        let (status, output) = wait_draining_stdout(child, Duration::from_millis(200), |c| {
            killed = true;
            let _ = c.kill();
        })
        .unwrap();
        assert!(status.is_none(), "should have timed out");
        assert!(output.is_none(), "timeout path captures no output");
        assert!(killed, "on_timeout must fire to kill the hung child");
    }
}
