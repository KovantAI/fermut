//! pytest-compatible runner.
//!
//! Invokes `<exe> -x --tb=no -q [--hypothesis-seed=N] [<extra args>]
//! <mirrored-tests>` per mutant inside the per-worker project mirror managed
//! by `with_worker_mirror`. Exit 0 → mutant
//! survived; non-zero → killed; wall-clock past `--timeout` → timed out.
//!
//! `exe` is the framework executable — `pytest`, or `rstest` (a pytest-CLI-
//! compatible drop-in). Both accept the same flags and node-id positionals, so
//! one implementation drives either; only the program name differs.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use wait_timeout::ChildExt;

use super::exit::{anomaly_message, classify_exit, ExitVerdict, TestTool};
use super::process_group::{kill_group, with_new_process_group};
use super::{
    build_mirror, configure_mirror_cmd, mirror_pythonpath, run_baseline_with_timeout, run_patched,
    BaselineStatus, Runner,
};
use crate::config::IsolationMode;
use crate::filter::coverage::CoverageContexts;
use crate::mutator::Mutant;
use crate::report::MutantOutcome;
use crate::sync::lock_recover;

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
    /// Smart test ordering on. When set, coverage-selected ids are reordered —
    /// the cold-start breadth prior (fewest of the mutated file's lines) first,
    /// then any learned `(file, operator)` killer lifted ahead of that — and
    /// killers observed this run are learned into `kill_sink`.
    smart_order: bool,
    /// Immutable kill-order history, read to order selected tests (no lock).
    kill_order: Arc<crate::kill_order::KillOrder>,
    /// Kills learned this run, folded into the sidecar by the engine afterward.
    kill_sink: Arc<Mutex<Vec<crate::kill_order::KillRecord>>>,
    /// Kill-set recording on (the higher-order-mutant experiment). When set, the
    /// per-mutant command drops `-x` and always captures stdout, and every
    /// mutant's full kill-set is pushed to `kill_set_sink`.
    record_kill_sets: bool,
    /// Per-mutant kill-sets recorded this run, drained by the engine to JSONL.
    kill_set_sink: Arc<Mutex<Vec<super::kill_sets::KillSetRecord>>>,
    /// Base for the `(file, operator)` history key — the run's `source_root`.
    /// Mutant files are made relative to this so the sidecar survives
    /// moves/checkouts.
    key_base: PathBuf,
}

/// Construction parameters for [`PytestRunner`], bundled so the runner is
/// built from one value instead of a 13-argument call. Every field is sourced
/// off the runtime [`Config`] (or derived from it) by [`super::build`].
pub struct PytestConfig {
    pub tests: PathBuf,
    pub timeout: Duration,
    pub baseline_timeout: Duration,
    pub hypothesis_seed: Option<u64>,
    pub extra_args: Vec<String>,
    pub isolation: IsolationMode,
    pub coverage: Option<Arc<CoverageContexts>>,
    pub python: Option<PathBuf>,
    pub exe: &'static str,
    pub smart_order: bool,
    pub kill_order: Arc<crate::kill_order::KillOrder>,
    pub kill_sink: Arc<Mutex<Vec<crate::kill_order::KillRecord>>>,
    pub record_kill_sets: bool,
    pub kill_set_sink: Arc<Mutex<Vec<super::kill_sets::KillSetRecord>>>,
    pub key_base: PathBuf,
}

impl PytestRunner {
    pub fn new(config: PytestConfig) -> Self {
        let PytestConfig {
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
            record_kill_sets,
            kill_set_sink,
            key_base,
        } = config;
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
            record_kill_sets,
            kill_set_sink,
            key_base,
        }
    }

    /// `(source-root-relative file, operator name)` history key for a mutant.
    /// The file is stripped of `key_base` (the run's `source_root`).
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
    /// kill-order sidecar. Best-effort: any misfire (no failure line,
    /// unparsable) just forfeits this datapoint. A poisoned sink is recovered
    /// via [`lock_recover`] so a panicked worker can't silently stop ordering
    /// from learning for the rest of the run.
    fn record_killer(&self, output: &str, file: &str, op: &'static str) {
        if let Some(nodeid) = crate::kill_order::parse_first_failed(output) {
            lock_recover(&self.kill_sink).push(crate::kill_order::KillRecord {
                file: file.to_string(),
                operator: op.to_string(),
                nodeid,
            });
        }
    }

    /// Cold-start breadth ordering of the coverage-selected node ids: with smart
    /// ordering the test most focused on the mutated file (fewest of its lines
    /// covered) leads so `-x` short-circuits sooner; otherwise the caller's
    /// coverage order is kept. Only permutes `ids` — never adds or drops — so the
    /// verdict is unchanged. Learned `(file, operator)` history is layered on top
    /// of this order by [`select_args`], so breadth fills the no-history gap.
    fn ordered_ids<'a>(
        &self,
        ctx: &CoverageContexts,
        mutant: &Mutant,
        ids: &'a [String],
    ) -> Vec<&'a String> {
        if self.smart_order {
            ctx.order_by_breadth_in(&mutant.file, ids)
        } else {
            ids.iter().collect()
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

    /// Assemble the per-mutant test command inside `mirror`: base flags, the
    /// coverage/smart-ordering test selection (or whole-dir sweep), working
    /// directory, PYTHONPATH/pyc env, and stdio. Returns the ready-to-spawn
    /// `Command` plus whether its stdout is captured (to learn the killer).
    fn build_mutant_command(
        &self,
        mirror: &super::Mirror,
        mutant: &Mutant,
        key_file: &str,
        key_op: &'static str,
    ) -> Result<Command> {
        let mut cmd = self.framework_command();
        cmd.arg("--tb=no").arg("-q");
        // `-x` stops at the first failing test — all a Killed/Survived verdict
        // needs. Kill-set recording needs EVERY covering test that fails, so drop
        // `-x` when recording (at the cost of running the full selected set on a
        // killed mutant instead of short-circuiting).
        if !self.record_kill_sets {
            cmd.arg("-x");
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
        // Smart ordering reorders those ids without changing the set, so the
        // mutation score is identical. (Under a timeout, reaching the killer
        // sooner can convert a would-be `timed_out` into a `killed` — but
        // both count as detected, so the score is unmoved; ordering only
        // ever changes speed.) Two layers: the cold-start breadth
        // prior (`ordered_ids`) puts the most *targeted* test — fewest of the
        // mutated file's lines — first, then [`select_args`] lifts any learned
        // `(file, operator)` killer ahead of that. Breadth fills the no-history
        // gap; history dominates once a killer is known.
        let ordered: Option<Vec<String>> = self.coverage.as_ref().and_then(|ctx| {
            ctx.tests_for_mutant(mutant).map(|ids| {
                self.ordered_ids(ctx, mutant, ids)
                    .into_iter()
                    .cloned()
                    .collect()
            })
        });
        let capture = match select_args(
            ordered.as_deref(),
            self.smart_order,
            &self.kill_order,
            key_file,
            key_op,
        ) {
            Selection::Ids { mut args, capture } => {
                // Recording forces capture (we parse the failing nodeids from the
                // `-rfE` summary); add `-rfE` if smart-order selection didn't.
                let capture = capture || self.record_kill_sets;
                if capture && !args.iter().any(|a| a == "-rfE") {
                    args.push("-rfE".to_string());
                }
                for a in &args {
                    cmd.arg(a);
                }
                capture
            }
            Selection::Sweep => {
                cmd.arg(&mirror.tests);
                // No coverage selection, but recording still needs the failure
                // summary from the whole-suite sweep.
                if self.record_kill_sets {
                    cmd.arg("-rfE");
                    true
                } else {
                    false
                }
            }
        };
        configure_mirror_cmd(&mut cmd, mirror)?;
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
        Ok(cmd)
    }

    /// Record this mutant's full kill-set (the covering tests that failed) into
    /// the shared sink for the engine to write out. Only called under
    /// `--record-kill-sets`, where `-x` is dropped and stdout always captured, so
    /// the `-rfE` summary lists every failure — the mutant's `K(m)`. The verdict
    /// is derived the same way [`classify_run`] derives it, so the recorded
    /// `status` matches the report; `kill_set` is populated only on a real kill.
    fn record_kill_set(
        &self,
        mutant: &Arc<Mutant>,
        waited: &(Option<ExitStatus>, Option<String>),
        key_file: &str,
        key_op: &'static str,
    ) {
        let (status, output) = waited;
        let (verdict, kill_set) = match status {
            Some(s) => match classify_exit(TestTool::Pytest, s.code()) {
                ExitVerdict::Survived => ("survived", Vec::new()),
                ExitVerdict::Killed => {
                    let mut ids = output
                        .as_deref()
                        .map(crate::kill_order::parse_all_failed)
                        .unwrap_or_default();
                    // Dedupe preserving first-seen order: a parametrized test can
                    // list the same nodeid once per failing param, but the
                    // kill-set is a set of tests.
                    let mut seen = std::collections::HashSet::new();
                    ids.retain(|id| seen.insert(id.clone()));
                    ("killed", ids)
                }
                ExitVerdict::Anomaly(_) => ("error", Vec::new()),
            },
            None => ("timed_out", Vec::new()),
        };
        lock_recover(&self.kill_set_sink).push(super::kill_sets::KillSetRecord {
            mutant_id: mutant.id.clone(),
            file: key_file.to_string(),
            operator: key_op.to_string(),
            line: mutant.line,
            status: verdict,
            kill_set,
        });
    }

    /// Turn a completed (or timed-out) test run into a `MutantOutcome`. On a
    /// real kill and captured output, records the killing node id for the next
    /// run's ordering. Anomalous exits become `error` (excluded from the score).
    fn classify_run(
        &self,
        mutant: &Arc<Mutant>,
        waited: (Option<ExitStatus>, Option<String>),
        key_file: &str,
        key_op: &'static str,
    ) -> MutantOutcome {
        match waited {
            (Some(status), output) => match classify_exit(TestTool::Pytest, status.code()) {
                ExitVerdict::Survived => MutantOutcome::survived(mutant.clone()),
                // A real kill (test failed, or a mutant that broke import so
                // pytest couldn't collect). When capturing, learn which test
                // did it and record it for next time's ordering.
                ExitVerdict::Killed => {
                    if let Some(out) = output {
                        self.record_killer(&out, key_file, key_op);
                    }
                    MutantOutcome::killed(mutant.clone())
                }
                // Not a verdict — surface as Error, excluded from the score
                // denominator so a stale node id can't manufacture a fake kill.
                ExitVerdict::Anomaly(code) => {
                    MutantOutcome::error(mutant.clone(), anomaly_message(TestTool::Pytest, code))
                }
            },
            (None, _) => MutantOutcome::timed_out(mutant.clone()),
        }
    }
}

/// Whether to capture the per-mutant pytest stdout to learn the killing test.
/// Only worth it when smart ordering is on AND more than one test was selected:
/// with a single (or zero) selected test there's nothing to reorder next run, so
/// the pipe + read + `-rfE` are pure overhead. Kept pure so the wiring is tested.
#[must_use]
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
#[must_use]
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

/// How long to wait for the stdout drain to *settle* once the direct child has
/// exited. An exited child has already flushed and closed its own write end, so
/// its bytes are sitting in the pipe and the reader EOFs after a microsecond
/// memcpy — this only needs to cover reading whatever is buffered, even a large
/// flood. If it overruns, the write end is still open, which after the leader
/// exited means a *grandchild* inherited it: the process group is therefore
/// still alive, so it is safe to release the pipe by killing the group. Kept
/// short so a test that leaks a background subprocess costs ~this, not the full
/// [`DRAIN_GRACE`], per captured mutant.
const DRAIN_SETTLE: Duration = Duration::from_millis(500);

/// Upper bound on the *final* collect after the process group has been killed
/// (grandchild reaped, pipe closing). Only a backstop — the reader EOFs almost
/// immediately once the write end is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Wait for `child` with `timeout` while concurrently draining its stdout on a
/// dedicated thread. Draining concurrently is load-bearing: if the child fills
/// its stdout pipe (~64 KiB) it blocks on `write`, which would wedge
/// `wait_timeout` forever and turn a kill into a false timeout — silently
/// changing the verdict. `on_timeout` terminates the child's process group;
/// it fires on the timeout path, and also on the rare grace-overrun path below.
/// Returns the exit status (`None` = timed out) and the captured stdout (`None`
/// when stdout wasn't piped, or when even the group-kill couldn't recover it).
///
/// The drain result is delivered over a channel, not a bare `JoinHandle::join`,
/// so the wait for it is *bounded*: if the child exits but a grandchild still
/// holds the pipe's write end the read never EOFs, and joining would wedge this
/// worker forever. We wait only [`DRAIN_SETTLE`] for the drain to finish; on
/// overrun (⇒ a grandchild holds the pipe) we call `on_timeout` (the same
/// process-group kill used on the timeout path) to reap the lingering grandchild
/// — closing the write end so the reader EOFs and its thread exits instead of
/// leaking for the worker's lifetime — then make one final collect bounded by
/// [`DRAIN_GRACE`], which usually *recovers* the datapoint the child already
/// wrote. Bytes are decoded lossily rather than via `read_to_string` so a single
/// non-UTF-8 byte can't discard the whole capture.
pub(crate) fn wait_draining_stdout(
    child: Child,
    timeout: Duration,
    on_timeout: impl FnMut(&mut Child),
) -> Result<(Option<ExitStatus>, Option<String>)> {
    wait_draining_stdout_with_grace(child, timeout, DRAIN_SETTLE, DRAIN_GRACE, on_timeout)
}

/// [`wait_draining_stdout`] with the settle and grace injected, so a test can
/// prove the bounds hold without waiting the full [`DRAIN_SETTLE`]/[`DRAIN_GRACE`].
fn wait_draining_stdout_with_grace(
    mut child: Child,
    timeout: Duration,
    settle: Duration,
    grace: Duration,
    mut on_timeout: impl FnMut(&mut Child),
) -> Result<(Option<ExitStatus>, Option<String>)> {
    // `eof_reached` is flipped by the reader thread the instant `read_to_end`
    // returns — which happens only when *every* write end of the pipe is
    // closed. It is the provable "pipe is closed" signal the success arm needs:
    // an atomic load can't lag the way an `mpsc` delivery can under a loaded
    // scheduler, so `!eof_reached` after the settle means a write end is still
    // open (a grandchild inherited it), not merely that the drained string is
    // slow to arrive over the channel.
    let eof_reached = Arc::new(AtomicBool::new(false));
    let mut reader = child.stdout.take().map(|mut out| {
        let (tx, rx) = std::sync::mpsc::channel();
        let eof_reached = Arc::clone(&eof_reached);
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            // Record EOF before the (potentially large) lossy decode + send, so
            // the flag reflects "pipe closed" the moment it is true, regardless
            // of how long delivery takes.
            eof_reached.store(true, Ordering::Release);
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        rx
    });
    let had_pipe = reader.is_some();
    // Take the drained output within `wait`. On overrun the receiver is left in
    // place (`reader` untouched) so a later attempt can still reap it; on success
    // it's consumed. `None` when stdout wasn't piped or `wait` elapsed.
    let collect =
        |reader: &mut Option<std::sync::mpsc::Receiver<String>>, wait: Duration| match reader {
            Some(rx) => match rx.recv_timeout(wait) {
                Ok(s) => {
                    *reader = None;
                    Some(s)
                }
                Err(_) => None,
            },
            None => None,
        };

    match child
        .wait_timeout(timeout)
        .context("waiting on test runner")?
    {
        Some(status) => {
            // Only the short settle here — an exited leader's own output drains
            // in microseconds, so an *un-EOF'd* pipe past the settle means a
            // grandchild holds the write end (group still alive), not a slow
            // normal read.
            let mut output = collect(&mut reader, settle);
            // Two distinct reasons the collect can come back `None`:
            //
            //   1. A grandchild inherited the write end, so `read_to_end` is
            //      still blocked and `eof_reached` is false. The group is alive:
            //      kill it to release the pipe (also reaping the stray
            //      subprocess), then make one final bounded collect — this closes
            //      the reader's EOF so its thread exits instead of leaking, and
            //      usually recovers the datapoint the child already wrote.
            //   2. The pipe already EOF'd (`eof_reached` true) but the drained
            //      string is merely lagging over the channel on a loaded box.
            //      `kill(-pgid)` MUST NOT fire: `wait_timeout` already reaped the
            //      leader, freeing its pid, and if no member keeps the pgid
            //      reserved the id can be recycled — the kill would land on an
            //      unrelated group. Just wait out the grace for the lagging
            //      bytes; no signal.
            //
            //      Accepted cost: EOF means every write end is *closed*, but a
            //      grandchild that closed its stdout copy could still be running
            //      (and would keep the pgid reserved, making a kill technically
            //      safe *here*). We still don't kill: we can't distinguish that
            //      from the recycled-pgid case without a `kill(-pgid, 0)` probe
            //      that carries the identical TOCTOU race. Such a grandchild is
            //      left to run to completion rather than risk signalling a
            //      recycled group. Do NOT add a kill back on this branch.
            //
            // The `eof_reached` load is what makes case 2 *provable* rather than
            // inferred from timing — an atomic can't lag like the mpsc delivery.
            // (Contrast the timeout arm below, which kills *before* the child is
            // reaped, so recycling is impossible there.)
            if output.is_none() && had_pipe {
                if eof_reached.load(Ordering::Acquire) {
                    output = collect(&mut reader, grace);
                } else {
                    on_timeout(&mut child);
                    let _ = child.wait();
                    output = collect(&mut reader, grace);
                }
            }
            Ok((Some(status), output))
        }
        None => {
            on_timeout(&mut child);
            let _ = child.wait();
            // Reap the drain now that the pipe has closed (bounded like above).
            let _ = collect(&mut reader, grace);
            Ok((None, None))
        }
    }
}

impl Runner for PytestRunner {
    fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
        std::mem::take(&mut *lock_recover(&self.kill_sink))
    }

    fn take_kill_sets(&self) -> Vec<super::kill_sets::KillSetRecord> {
        std::mem::take(&mut *lock_recover(&self.kill_set_sink))
    }

    fn run(&self, mutant: &Arc<Mutant>) -> Result<MutantOutcome> {
        run_patched(&self.tests, self.isolation, mutant, |mirror| {
            let (key_file, key_op) = self.kill_key(mutant);
            let mut cmd = self.build_mutant_command(mirror, mutant, &key_file, key_op)?;
            let child = cmd.spawn().context("spawning test runner")?;
            let waited = wait_draining_stdout(child, self.timeout, kill_group)?;
            if self.record_kill_sets {
                self.record_kill_set(mutant, &waited, &key_file, key_op);
            }
            Ok(self.classify_run(mutant, waited, &key_file, key_op))
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
    fn wait_draining_reaps_grandchild_holding_stdout_and_recovers() {
        // Regression for the bounded-drain fix: the direct child exits, but a
        // backgrounded grandchild inherited the stdout write end, so the read
        // never EOFs. An unbounded `join` would wedge the worker forever *and*
        // leak the reader thread. Instead, once the grace elapses we kill the
        // process group (via `on_timeout`) to reap the grandchild — closing the
        // pipe so the reader EOFs and its thread exits — then make one final
        // collect that recovers the `done` the child already wrote.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo done") // shell exits; the `sleep` keeps stdout open
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        with_new_process_group(&mut cmd); // so kill_group reaches the `sleep`
        let child = cmd.spawn().expect("spawn sh");

        let settle = Duration::from_millis(150);
        let grace = Duration::from_millis(300);
        let mut fired = 0;
        let start = std::time::Instant::now();
        let (status, output) =
            wait_draining_stdout_with_grace(child, Duration::from_secs(30), settle, grace, |c| {
                fired += 1;
                kill_group(c); // reap the lingering grandchild → releases the pipe
            })
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            status.expect("child exited, not timed out").success(),
            "the direct child exited 0"
        );
        assert_eq!(
            fired, 1,
            "grace overrun must fire the group-kill exactly once"
        );
        assert_eq!(
            output.as_deref().map(str::trim),
            Some("done"),
            "reaping the grandchild must recover the child's output, got {output:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "must return within ~2×grace, not block on the never-EOFing pipe (took {elapsed:?})"
        );
    }

    #[test]
    #[cfg(unix)]
    fn wait_draining_leaked_subprocess_bounded_by_settle_not_grace() {
        // Finding #2 regression guard: a test that leaks a background
        // subprocess holding stdout must cost only ~DRAIN_SETTLE per captured
        // mutant, NOT the full DRAIN_GRACE. Prove it with a SMALL settle and a
        // LARGE grace: the first collect must overrun `settle` and fire the
        // group-kill promptly, so the whole call returns on the order of
        // `settle` even though `grace` is many times larger. A regression that
        // waited `grace` before killing (the old single-timeout behavior) would
        // blow this bound.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo done")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        with_new_process_group(&mut cmd);
        let child = cmd.spawn().expect("spawn sh");

        let settle = Duration::from_millis(100);
        let grace = Duration::from_secs(5); // deliberately >> settle
        let start = std::time::Instant::now();
        let (status, output) =
            wait_draining_stdout_with_grace(child, Duration::from_secs(30), settle, grace, |c| {
                kill_group(c);
            })
            .unwrap();
        let elapsed = start.elapsed();

        assert!(status.expect("child exited").success());
        assert_eq!(output.as_deref().map(str::trim), Some("done"));
        // The kill fires after `settle`; the post-kill collect EOFs at once. Must
        // be far below `grace` — generous headroom for a loaded CI scheduler.
        assert!(
            elapsed < Duration::from_secs(2),
            "leaked-subprocess drain must be bounded by settle ({settle:?}), not grace \
             ({grace:?}); took {elapsed:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn wait_draining_lagging_drain_without_grandchild_does_not_group_kill() {
        // The drain-overrun-race fix. The direct child writes a large flood and
        // exits with NO grandchild, so the pipe's write end is already closed —
        // but the reader's lossy-decode of ~30 MiB takes longer than a tiny
        // `settle`, so the first collect times out. The OLD code read that
        // timeout as "a grandchild holds the pipe" and fired the group-kill;
        // since `wait_timeout` had already reaped the leader (freeing its pid),
        // that `kill(-pgid)` could land on a recycled process group. The fix
        // gates the kill on `eof_reached`: `read_to_end` has already returned
        // (EOF observed, flag set before the slow decode), so the kill MUST NOT
        // fire — the lagging bytes are simply collected over the grace.
        //
        // `settle` is tiny (decode outlasts it → first collect misses) but the
        // residual read after the leader exits is only a pipe-buffer's worth
        // (~microseconds), so `eof_reached` is reliably true by the time the
        // branch is reached.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("head -c 31457280 /dev/zero | tr '\\0' a") // 30 MiB of 'a', no grandchild
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        with_new_process_group(&mut cmd); // group exists, but nothing survives the leader
        let child = cmd.spawn().expect("spawn sh flooder");

        let settle = Duration::from_millis(2);
        let grace = Duration::from_secs(10);
        let (status, output) =
            wait_draining_stdout_with_grace(child, Duration::from_secs(30), settle, grace, |_| {
                panic!("group-kill must not fire when the pipe is already EOF'd (no grandchild)");
            })
            .unwrap();

        assert!(status.expect("child exited, not timed out").success());
        let out = output.expect("lagging drain must still be recovered over the grace");
        assert_eq!(
            out.len(),
            31_457_280,
            "the full flood must be captured without a group-kill"
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

    #[test]
    #[cfg(unix)]
    fn timeout_makes_the_verdict_order_sensitive() {
        // Guards the corrected docs claim: ordering never changes kill/survive,
        // but WITH a per-mutant timeout the killed↔timed_out verdict *is*
        // order-sensitive — which is exactly why smart ordering (killer first)
        // helps, and why enabling it can flip a borderline mutant.
        //
        // `runner` emulates pytest `-x`: it runs its positional "tests" in the
        // given order, `slow` sleeps past the timeout (a passing-but-slow test),
        // `fail` exits non-zero immediately (the killing test, short-circuiting).
        // Same test SET, two orders, one short timeout → two different verdicts.
        let runner = "for t in \"$@\"; do \
                        case $t in \
                          slow) sleep 5;; \
                          fail) exit 1;; \
                        esac; \
                      done; exit 0";
        let spawn = |order: &[&str]| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(runner).arg("sh"); // $0 then positionals
            for t in order {
                cmd.arg(t);
            }
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            with_new_process_group(&mut cmd); // so kill_group reaps the `sleep`
            cmd.spawn().expect("spawn sh runner")
        };
        let timeout = Duration::from_millis(300);

        // Killer first → `-x` short-circuits before the slow test → KILLED
        // (non-zero exit within the timeout).
        let (status, _) =
            wait_draining_stdout(spawn(&["fail", "slow"]), timeout, kill_group).unwrap();
        let killer_first = status.expect("killer-first must exit, not time out");
        assert!(
            !killer_first.success(),
            "killer-first is a kill (non-zero exit)"
        );

        // Killer last → the slow test runs first and blows the timeout before the
        // killer is ever reached → TIMED_OUT. Same set, opposite verdict.
        let (status, _) =
            wait_draining_stdout(spawn(&["slow", "fail"]), timeout, kill_group).unwrap();
        assert!(
            status.is_none(),
            "killer-last must time out before reaching the killer"
        );
    }

    // --- ordered_ids: locks the cold-start breadth prior (main #63).

    use std::io::Write;
    use std::path::Path;

    // wide covers 3 lines, mid 1, narrow 1 — same fixture as the coverage tests.
    fn breadth_ctx(dir: &Path) -> Arc<CoverageContexts> {
        std::fs::write(dir.join("foo.py"), "a = 1\nb = 2\nc = 3\n").unwrap();
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/t.py::wide|run"],
                        "2": ["tests/t.py::wide|run", "tests/t.py::narrow|run"],
                        "3": ["tests/t.py::wide|run", "tests/t.py::mid|run"]
                    }
                }
            }
        }"#;
        let path = dir.join("coverage.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(doc.as_bytes())
            .unwrap();
        CoverageContexts::from_json(&path, dir, dir).unwrap()
    }

    fn runner(smart_order: bool, coverage: Arc<CoverageContexts>) -> PytestRunner {
        PytestRunner::new(PytestConfig {
            tests: PathBuf::from("tests"),
            timeout: Duration::from_secs(30),
            baseline_timeout: Duration::from_secs(300),
            hypothesis_seed: None,
            extra_args: Vec::new(),
            isolation: IsolationMode::Auto,
            coverage: Some(coverage),
            python: None,
            exe: "pytest",
            smart_order,
            kill_order: Arc::new(crate::kill_order::KillOrder::default()),
            kill_sink: Arc::new(Mutex::new(Vec::new())),
            record_kill_sets: false,
            kill_set_sink: Arc::new(Mutex::new(Vec::new())),
            key_base: PathBuf::from("."),
        })
    }

    fn selected() -> Vec<String> {
        ["tests/t.py::wide", "tests/t.py::narrow", "tests/t.py::mid"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    // A mutant in `foo.py` — the file the breadth fixture indexes — so scoped
    // ordering resolves to that file's per-test breadth.
    fn mutant_in(dir: &Path) -> Mutant {
        Mutant {
            id: "id".into(),
            file: dir.join("foo.py"),
            operator: crate::mutator::Operator::ArithOpSwap,
            range: ruff_text_size::TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn ordered_ids_smart_puts_narrowest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = breadth_ctx(tmp.path());
        let r = runner(true, ctx.clone());
        let ids = selected();
        let m = mutant_in(tmp.path());
        let out: Vec<String> = r.ordered_ids(&ctx, &m, &ids).into_iter().cloned().collect();
        // wide (breadth 3) sinks; the two breadth-1 tests keep input order (stable).
        assert_eq!(
            out,
            vec![
                "tests/t.py::narrow".to_string(),
                "tests/t.py::mid".to_string(),
                "tests/t.py::wide".to_string(),
            ]
        );
    }

    #[test]
    fn ordered_ids_no_smart_keeps_coverage_order() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = breadth_ctx(tmp.path());
        let r = runner(false, ctx.clone());
        let ids = selected();
        let m = mutant_in(tmp.path());
        let out: Vec<String> = r.ordered_ids(&ctx, &m, &ids).into_iter().cloned().collect();
        // Smart ordering off → caller's coverage order is preserved verbatim.
        assert_eq!(out, ids);
    }

    #[test]
    fn ordered_ids_is_a_permutation_either_way() {
        // Verdict-invariance: both branches return exactly the selected set,
        // only the order differs — so pytest's `-x` runs the same tests.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = breadth_ctx(tmp.path());
        let ids = selected();
        let m = mutant_in(tmp.path());
        for smart in [true, false] {
            let r = runner(smart, ctx.clone());
            let mut out: Vec<String> = r.ordered_ids(&ctx, &m, &ids).into_iter().cloned().collect();
            let mut want = ids.clone();
            out.sort();
            want.sort();
            assert_eq!(
                out, want,
                "smart={smart} must be a permutation of the input"
            );
        }
    }

    // The exit-code → verdict mapping and anomaly messages now live in
    // `runner::exit` and are unit-tested there (both tools in one place).
}
