//! Schemata mode for the pytest runner (`--schemata`, experimental).
//!
//! Instead of splicing one mutant into a mirror and starting pytest per
//! mutant, every function-body mutant is compiled into one *schema* up front:
//! each mutated function gets a switch on an active-mutant index (see
//! `python/pytest_fermut/_fermut_schema.py`). Each rayon worker then keeps a
//! *template* — a pytest process that imported and collected the suite once —
//! and runs a mutant by asking it to fork a child with that index active. The
//! child runs the same selected tests a spawned pytest would, in the same
//! order, and the verdict goes through the same classification as the spawn
//! path.
//!
//! Anything the schema can't serve safely runs on the wrapped
//! [`PytestRunner`] exactly as without `--schemata`: mutants outside function
//! bodies, ones that would warn at compile time, functions that already ran
//! inside the template (their results may have been stored before the fork),
//! a template that failed to start or died, and a child that broke itself.
//! The baseline is always the cold, spawn-path run.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use super::pytest::{run_timing, PytestRunner};
use super::pytest_result::RunResult;
use super::{configure_mirror_cmd, interpreter, BaselineStatus, Mirror, Runner};
use crate::mutator::Mutant;
use crate::report::MutantOutcome;
use crate::sync::lock_recover;

/// The helper script, written next to the schema mirror.
const SCRIPT_SOURCE: &str = include_str!("../../python/pytest_fermut/_fermut_schema.py");

/// Waiting for a template's `{"pid"}` after a request: a fork is instant, so
/// missing this means the template is wedged.
const FORK_TIMEOUT: Duration = Duration::from_secs(30);

/// Waiting for the result after killing a timed-out child's group: the
/// template only has to reap it.
const REAP_GRACE: Duration = Duration::from_secs(5);

pub struct SchemaRunner {
    inner: PytestRunner,
    /// Mutants handed over by [`Runner::prepare`]; the schema is built from
    /// them on the first `run`, so a fully cached run never pays for it.
    mutants: Mutex<Vec<Arc<Mutant>>>,
    schema: OnceLock<Option<Schema>>,
    /// Set by the worker that builds the schema; the others run their
    /// mutants normally meanwhile instead of waiting on it.
    building: AtomicBool,
    /// [`Self::confirm`]'s verdict per test list.
    confirmed: Mutex<HashMap<Vec<String>, bool>>,
    /// Mutants run from the schema vs on the spawn path, for the end-of-run
    /// summary.
    served: AtomicU64,
    spawned: AtomicU64,
}

/// The built schema: one read-only mirror shared by every template.
struct Schema {
    /// Distinguishes this schema from one built by an earlier run in the same
    /// process (watch mode), so a worker never reuses a stale template.
    generation: u64,
    mirror: Mirror,
    /// Mutant id → its index in the schema. Absent ids run on the spawn path.
    index: HashMap<String, u64>,
    /// Template command line after the interpreter: script, `serve`, args.
    serve_args: Vec<std::ffi::OsString>,
    python: PathBuf,
    /// How long a template may take to import and collect the suite; also
    /// bounds the verification run.
    ready_timeout: Duration,
    /// Whether the suite passes on the schema with no mutant active (see
    /// [`verify`]), checked one slice per worker.
    verified: Chunks,
}

#[derive(Deserialize)]
struct SchematizeOutput {
    mutants: HashMap<String, SchematizeEntry>,
    functions: serde_json::Value,
}

#[derive(Deserialize)]
struct SchematizeEntry {
    idx: Option<u64>,
    fallback: Option<String>,
}

impl SchemaRunner {
    pub fn new(inner: PytestRunner) -> Self {
        Self {
            inner,
            mutants: Mutex::new(Vec::new()),
            schema: OnceLock::new(),
            building: AtomicBool::new(false),
            confirmed: Mutex::new(HashMap::new()),
            served: AtomicU64::new(0),
            spawned: AtomicU64::new(0),
        }
    }

    /// The schema, built by the first worker to ask. `None` while another
    /// worker is still building it (run this mutant normally) or if it
    /// couldn't be built.
    fn schema(&self) -> Option<&Schema> {
        if let Some(schema) = self.schema.get() {
            return schema.as_ref();
        }
        if self.building.swap(true, Ordering::AcqRel) {
            return None;
        }
        let mutants = std::mem::take(&mut *lock_recover(&self.mutants));
        let built = match self.build_schema(&mutants) {
            Ok(schema) => Some(schema),
            Err(e) => {
                warn!(
                    "--schemata: couldn't build the schema, running every mutant normally: {e:#}"
                );
                None
            }
        };
        let _ = self.schema.set(built);
        self.schema.get().and_then(Option::as_ref)
    }

    fn build_schema(&self, mutants: &[Arc<Mutant>]) -> Result<Schema> {
        static GENERATION: AtomicU64 = AtomicU64::new(0);
        let mirror = Mirror::build(&self.inner.tests, self.inner.isolation)?;
        let script = mirror.plugin_dir.join("_fermut_schema.py");
        std::fs::write(&script, SCRIPT_SOURCE)
            .with_context(|| format!("writing {}", script.display()))?;
        let mut input = Vec::with_capacity(mutants.len());
        for m in mutants {
            let Ok(target) = mirror.target_for(&m.file) else {
                continue;
            };
            let Ok(rel) = target.strip_prefix(&mirror.root) else {
                continue;
            };
            input.push(serde_json::json!({
                "id": m.id,
                "file": rel,
                "start": u32::from(m.range.start()),
                "end": u32::from(m.range.end()),
                "replacement": m.replacement,
            }));
        }
        let python = pytest_interpreter(self.inner.python.as_deref());
        let mut cmd = Command::new(&python);
        cmd.arg(&script).arg("schematize").arg(&mirror.root);
        configure_mirror_cmd(&mut cmd, &mirror)?;
        let out = run_with_stdin(cmd, &serde_json::to_vec(&input)?)?;
        let parsed: SchematizeOutput =
            serde_json::from_slice(&out).context("parsing the schematize output")?;

        let mut index = HashMap::new();
        let mut fallbacks: HashMap<String, usize> = HashMap::new();
        for (id, entry) in parsed.mutants {
            match (entry.idx, entry.fallback) {
                (Some(idx), _) => {
                    index.insert(id, idx);
                }
                (None, why) => *fallbacks.entry(why.unwrap_or_default()).or_default() += 1,
            }
        }
        let mut reasons: Vec<_> = fallbacks.into_iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let detail: Vec<String> = reasons
            .iter()
            .map(|(why, n)| format!("{why} {n}"))
            .collect();
        info!(
            "schemata: {} mutants in the schema, {} outside it{}",
            index.len(),
            reasons.iter().map(|r| r.1).sum::<usize>(),
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", detail.join(", "))
            }
        );

        let schema_json = mirror.plugin_dir.join("schema.json");
        std::fs::write(
            &schema_json,
            serde_json::to_vec(&serde_json::json!({"functions": parsed.functions}))?,
        )
        .with_context(|| format!("writing {}", schema_json.display()))?;
        let mut serve_args: Vec<std::ffi::OsString> = vec![
            script.clone().into(),
            "serve".into(),
            mirror.root.clone().into(),
            schema_json.clone().into(),
        ];
        serve_args.extend(["--tb=no".into(), "-q".into()]);
        if let Some(seed) = self.inner.hypothesis_seed {
            serve_args.push(format!("--hypothesis-seed={seed}").into());
        }
        serve_args.extend(self.inner.extra_args.iter().map(Into::into));
        serve_args.push(mirror.tests.clone().into());
        Ok(Schema {
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
            mirror,
            index,
            serve_args,
            python,
            ready_timeout: self.inner.baseline_timeout,
            // One slice per worker: built inside the run's rayon pool.
            verified: Chunks::new(rayon::current_num_threads()),
        })
    }

    /// Run `mutant` in this worker's template. `None` → run it on the spawn
    /// path instead (not in the schema, unsafe, or no usable template).
    fn serve(&self, mutant: &Arc<Mutant>, only: Option<&str>, all: bool) -> Option<Served> {
        let schema = self.schema()?;
        let idx = *schema.index.get(&mutant.id)?;
        let (key_file, key_op) = self.inner.kill_key(mutant);
        let ids = self.inner.coverage_ids(mutant, only).map(|ids| {
            if self.inner.smart_order {
                self.inner.kill_order.order(&ids, &key_file, key_op)
            } else {
                ids
            }
        });
        if schema.verified.failed() {
            return None;
        }
        with_template(schema, |template| {
            // Until every slice of the suite has passed with no mutant active,
            // check one more slice if any is left unclaimed, then run this
            // mutant normally: no worker waits on another's slice.
            if !schema.verified.passed() {
                if let Some(k) = schema.verified.claim() {
                    let ok = verify(template, (k, schema.verified.n), schema.ready_timeout);
                    schema.verified.finish(ok);
                }
                if !schema.verified.passed() {
                    return Ok(None);
                }
            }
            if template.is_unsafe(idx) {
                return Ok(None);
            }
            let mut served = template.request(idx, ids.as_deref(), all, self.inner.timeout)?;
            // A kill (exit 1) or a timeout counts as detected: either must be
            // the mutant's doing.
            if served.status.is_none() || served.status.and_then(|s| s.code()) == Some(1) {
                served.unconfirmed = !self.confirm(template, &served.ran);
            }
            Ok(Some(served))
        })
    }

    /// Whether a kill is the mutant's: the same tests, in the same order, pass
    /// in a fork with no mutant active. A test can fail in a fork alone, from
    /// state another test module left behind while the template collected the
    /// suite (agent-library: a manifest scan at import emptied a registry that
    /// only a test's teardown restores) — a cold run never imports that
    /// module, so the kill would be false. Cached per test list: most kills
    /// come from the first selected test.
    fn confirm(&self, template: &mut Template, ran: &[String]) -> bool {
        if let Some(&ok) = lock_recover(&self.confirmed).get(ran) {
            return ok;
        }
        let ok = matches!(
            template.request(0, Some(ran), false, self.inner.timeout),
            Ok(s) if s.error.is_none() && s.status.is_some_and(|st| st.success())
        );
        if !ok {
            warn_unconfirmed(ran);
        }
        lock_recover(&self.confirmed).insert(ran.to_vec(), ok);
        ok
    }

    /// Turn a served run into the outcome the spawn path would have produced.
    fn outcome(&self, mutant: &Arc<Mutant>, served: Served, record: bool) -> MutantOutcome {
        let (key_file, key_op) = self.inner.kill_key(mutant);
        let waited = (served.status, None);
        if record && self.inner.record_kill_sets {
            self.inner.record_kill_set(
                mutant,
                &waited,
                Some(&served.result),
                false,
                &key_file,
                key_op,
            );
        }
        self.inner
            .classify_run(
                mutant,
                waited,
                Some(&served.result),
                false,
                None,
                &key_file,
                key_op,
            )
            .with_timing(run_timing(served.wall, Some(&served.result)))
    }
}

impl Drop for SchemaRunner {
    fn drop(&mut self) {
        let (served, spawned) = (*self.served.get_mut(), *self.spawned.get_mut());
        if served + spawned > 0 {
            info!("schemata: {served} mutants run from the schema, {spawned} run normally");
        }
    }
}

/// Run slice `chunk = (k, n)` of the suite on the schema with no mutant
/// active (every slice, across the workers, covers the suite), every test even
/// past a failure. Tests that fail, crash or hang here don't turn schemata
/// off: a kill or timeout from the schema only stands once the same tests pass
/// with no mutant active ([`SchemaRunner::confirm`]), so they just cost those
/// mutants a normal run. This reports them, and resumes a slice past a test
/// that took its child down (a crash: macOS DNS, proxy and Kerberos frameworks
/// aren't fork-safe; or a hang). Only a broken helper (a traceback) fails it.
fn verify(template: &mut Template, chunk: (usize, usize), timeout: Duration) -> bool {
    let mut req = serde_json::json!({"idx": 0, "chunk": chunk, "all": true});
    let (mut failing, mut lost) = (Vec::new(), Vec::new());
    let result = loop {
        match template.send(&req, timeout) {
            Ok(s) if s.error.is_none() => {
                failing.extend(s.result.failures.iter().cloned());
                let died = s.status.is_none_or(crash_signal);
                match s.result.unfinished {
                    // A repeat means the resume didn't skip it: stop there.
                    Some(test) if died && !lost.contains(&test) => {
                        req["after"] = test.clone().into();
                        lost.push(test);
                    }
                    _ => break Ok(()),
                }
            }
            Ok(s) => break Err(s.error.unwrap_or_default()),
            Err(e) => break Err(format!("{e:#}")),
        }
    };
    failing.retain(|t| !lost.contains(t));
    if !failing.is_empty() || !lost.is_empty() {
        info!(
            "schemata: with no mutant active, {} test(s) fail and {} crash or hang in a forked \
             run ({}); mutants they kill run normally",
            failing.len(),
            lost.len(),
            failing
                .iter()
                .chain(&lost)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    match result {
        Ok(()) => true,
        Err(why) => {
            // Once: with the check split across workers, several slices can fail.
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                warn!("--schemata: the schema helper failed checking the suite ({why}); running every mutant normally. Please report this.");
            }
            false
        }
    }
}

/// The no-mutant check, split into one slice of the suite per worker so no
/// worker runs the whole suite while the rest wait (typer: ~60 s).
struct Chunks {
    n: usize,
    next: AtomicUsize,
    done: AtomicUsize,
    failed: AtomicBool,
}

impl Chunks {
    fn new(n: usize) -> Self {
        Self {
            n: n.max(1),
            next: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
        }
    }

    /// A slice nobody has checked yet, if any.
    fn claim(&self) -> Option<usize> {
        let k = self.next.fetch_add(1, Ordering::AcqRel);
        (k < self.n).then_some(k)
    }

    fn finish(&self, ok: bool) {
        if !ok {
            self.failed.store(true, Ordering::Release);
        }
        self.done.fetch_add(1, Ordering::AcqRel);
    }

    /// Every slice checked, none failed.
    fn passed(&self) -> bool {
        !self.failed() && self.done.load(Ordering::Acquire) >= self.n
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// Whether a served run can stand as the mutant's verdict. The schema has no
/// say over a child that broke itself (exit 3 with a traceback) or a selection
/// the template didn't collect (exit 4: pytest would have been asked for ids
/// outside the tests path); the spawn path answers those.
fn usable(served: &Served) -> bool {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if let Some(tb) = &served.error {
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!("--schemata: a forked run failed internally; running such mutants normally. First failure:\n{tb}");
        }
        return false;
    }
    // A crashed child (a fork-unsafe system framework, macOS) isn't the
    // mutant's verdict either: pytest scores a signal death as a kill.
    !served.unconfirmed
        && !served.status.is_some_and(crash_signal)
        && served.status.and_then(|s| s.code()) != Some(4)
}

fn warn_unconfirmed(ran: &[String]) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "--schemata: {} fails in a forked run even with no mutant active; mutants it kills run normally",
            ran.join(", ")
        );
    }
}

/// Died of a crash signal. A timeout's kill isn't one: a timed-out run has no
/// status at all.
fn crash_signal(status: ExitStatus) -> bool {
    matches!(
        status.signal(),
        Some(
            libc::SIGSEGV
                | libc::SIGBUS
                | libc::SIGABRT
                | libc::SIGILL
                | libc::SIGFPE
                | libc::SIGTRAP
        )
    )
}

impl Runner for SchemaRunner {
    fn prepare(&self, mutants: &[Arc<Mutant>]) -> Result<()> {
        *lock_recover(&self.mutants) = mutants.to_vec();
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) {
        self.inner.set_timeout(timeout);
    }

    fn run(&self, mutant: &Arc<Mutant>) -> Result<MutantOutcome> {
        let all = self.inner.record_kill_sets;
        match self.serve(mutant, None, all) {
            Some(served) if usable(&served) => {
                self.served.fetch_add(1, Ordering::Relaxed);
                Ok(self.outcome(mutant, served, true))
            }
            _ => {
                self.spawned.fetch_add(1, Ordering::Relaxed);
                self.inner.run(mutant)
            }
        }
    }

    fn audit(&self, mutant: &Arc<Mutant>, killer: &str) -> Result<Option<MutantOutcome>> {
        match self.serve(mutant, Some(killer), false) {
            Some(served) if usable(&served) => Ok(Some(self.outcome(mutant, served, false))),
            _ => self.inner.audit(mutant, killer),
        }
    }

    fn baseline(&self) -> Result<BaselineStatus> {
        self.inner.baseline()
    }

    fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
        self.inner.take_kill_records()
    }

    fn take_kill_sets(&self) -> Vec<super::kill_sets::KillSetRecord> {
        self.inner.take_kill_sets()
    }
}

/// The interpreter the spawn path's pytest runs on: `--python` when given,
/// else the one in the shebang of the `pytest` the spawn path would find on
/// `PATH` (a different interpreter may not even have pytest installed).
fn pytest_interpreter(python: Option<&std::path::Path>) -> PathBuf {
    if let Some(p) = python {
        return p.to_path_buf();
    }
    which::which("pytest")
        .ok()
        .and_then(|script| std::fs::read(script).ok())
        .and_then(|bytes| shebang_interpreter(&bytes))
        .unwrap_or_else(|| interpreter(None))
}

/// `/path/to/python` from a `#!/path/to/python` first line, when it exists.
/// pip writes a `#!/bin/sh` trampoline instead for very long paths; that and
/// `#!/usr/bin/env python3` (no single interpreter path) give `None`.
fn shebang_interpreter(script: &[u8]) -> Option<PathBuf> {
    let first = script.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(first.strip_prefix(b"#!")?).ok()?.trim();
    let path = PathBuf::from(line.split_whitespace().next()?);
    let name = path.file_name()?.to_string_lossy();
    (name.starts_with("python") && path.is_file()).then_some(path)
}

/// Run `cmd` with `input` on stdin; its stdout on success.
fn run_with_stdin(mut cmd: Command, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting the schemata helper")?;
    let mut stdin = child.stdin.take().context("helper stdin")?;
    let input = input.to_vec();
    // Feed stdin from a thread: a large input can fill the pipe while the
    // helper is blocked writing stdout we haven't started reading.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child
        .wait_with_output()
        .context("waiting for the schemata helper")?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("stdin writer panicked"))??;
    if !out.status.success() {
        bail!(
            "schemata helper exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// One forked run, as the template reported it.
struct Served {
    /// `None` when it timed out (the child was killed).
    status: Option<ExitStatus>,
    result: RunResult,
    wall: Duration,
    /// Traceback when the child itself broke rather than a test failing.
    error: Option<String>,
    /// The tests the child ran, in order.
    ran: Vec<String>,
    /// A kill that the same tests reproduce with no mutant active: not the
    /// mutant's doing (see [`SchemaRunner::confirm`]).
    unconfirmed: bool,
}

#[derive(Deserialize)]
struct Ready {
    ready: bool,
    #[serde(default, rename = "unsafe")]
    unsafe_ranges: Vec<(u64, u64)>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct Pid {
    pid: u32,
}

#[derive(Deserialize)]
struct Done {
    status: i32,
    #[serde(default)]
    failed: Vec<String>,
    #[serde(default)]
    test_secs: f64,
    #[serde(default)]
    unfinished: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ran: Vec<String>,
}

/// A worker's template process and the protocol over its stdio.
struct Template {
    child: Child,
    stdin: ChildStdin,
    /// Lines from the template's stdout, read on a thread so every wait can
    /// have a deadline.
    lines: Receiver<String>,
    /// Index ranges of functions that ran inside the template before forking.
    unsafe_ranges: Vec<(u64, u64)>,
}

impl Template {
    fn start(schema: &Schema) -> Result<Self> {
        let mut cmd = Command::new(&schema.python);
        cmd.args(&schema.serve_args);
        configure_mirror_cmd(&mut cmd, &schema.mirror)?;
        cmd.env_remove("FM_ACTIVE");
        let log = tempfile::NamedTempFile::new_in(&schema.mirror.plugin_dir)
            .context("creating template log")?;
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log.as_file().try_clone()?)
            .spawn()
            .context("starting a schemata template")?;
        let stdin = child.stdin.take().context("template stdin")?;
        let stdout = child.stdout.take().context("template stdout")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut template = Self {
            child,
            stdin,
            lines,
            unsafe_ranges: Vec::new(),
        };
        let line = template.recv(schema.ready_timeout).with_context(|| {
            let stderr = std::fs::read_to_string(log.path()).unwrap_or_default();
            format!(
                "template never became ready; its stderr:\n{}",
                tail(&stderr, 40)
            )
        })?;
        let ready: Ready =
            serde_json::from_str(&line).context("parsing the template's ready line")?;
        if !ready.ready {
            bail!("template refused: {}", ready.error.unwrap_or_default());
        }
        template.unsafe_ranges = ready.unsafe_ranges;
        Ok(template)
    }

    fn is_unsafe(&self, idx: u64) -> bool {
        in_ranges(&self.unsafe_ranges, idx)
    }

    fn recv(&self, timeout: Duration) -> Result<String> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => Ok(line),
            Err(RecvTimeoutError::Timeout) => bail!("no answer within {timeout:?}"),
            Err(RecvTimeoutError::Disconnected) => bail!("the template exited"),
        }
    }

    fn request(
        &mut self,
        idx: u64,
        ids: Option<&[String]>,
        all: bool,
        timeout: Duration,
    ) -> Result<Served> {
        self.send(
            &serde_json::json!({"idx": idx, "ids": ids, "all": all}),
            timeout,
        )
    }

    fn send(&mut self, req: &serde_json::Value, timeout: Duration) -> Result<Served> {
        writeln!(self.stdin, "{req}").context("sending a request")?;
        self.stdin.flush().context("sending a request")?;
        let pid: Pid =
            serde_json::from_str(&self.recv(FORK_TIMEOUT)?).context("parsing the fork reply")?;
        let started = Instant::now();
        let (line, timed_out) = match self.recv(timeout) {
            Ok(line) => (line, false),
            Err(_) if !matches!(self.child.try_wait(), Ok(Some(_))) => {
                super::process_group::kill_pgid(pid.pid);
                (self.recv(REAP_GRACE)?, true)
            }
            Err(e) => return Err(e),
        };
        let wall = started.elapsed();
        let done: Done = serde_json::from_str(&line).context("parsing the run result")?;
        Ok(Served {
            status: (!timed_out).then(|| ExitStatus::from_raw(done.status)),
            result: RunResult {
                failures: done.failed,
                unfinished: done.unfinished,
                test_secs: done.test_secs,
                ..RunResult::default()
            },
            wall,
            error: done.error,
            ran: done.ran,
            unconfirmed: false,
        })
    }
}

impl Drop for Template {
    fn drop(&mut self) {
        // EOF on stdin ends it cleanly; the kill covers a wedged one.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum Slot {
    Live(Template),
    Dead,
}

thread_local! {
    /// This worker's template, tagged with the schema generation it serves.
    static TEMPLATE: RefCell<Option<(u64, Slot)>> = const { RefCell::new(None) };
}

/// Run `f` on this worker's template, starting one on first use. `None` →
/// run the mutant on the spawn path. A template that failed to start, or
/// whose request failed (it may be wedged, or its replies out of step), is
/// dropped for the rest of the run: one bad environment costs one warning,
/// not a retry per mutant.
fn with_template<T>(
    schema: &Schema,
    f: impl FnOnce(&mut Template) -> Result<Option<T>>,
) -> Option<T> {
    TEMPLATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if !matches!(&*slot, Some((generation, _)) if *generation == schema.generation) {
            let started = match Template::start(schema) {
                Ok(t) => Slot::Live(t),
                Err(e) => {
                    warn_once(&format!(
                        "a template couldn't start, its worker runs mutants normally: {e:#}"
                    ));
                    Slot::Dead
                }
            };
            *slot = Some((schema.generation, started));
        }
        let Some((_, Slot::Live(template))) = slot.as_mut() else {
            return None;
        };
        match f(template) {
            Ok(out) => out,
            Err(e) => {
                warn_once(&format!(
                    "a template stopped answering, its worker runs mutants normally: {e:#}"
                ));
                if let Some((_, s)) = slot.as_mut() {
                    *s = Slot::Dead; // drops (and kills) the template
                }
                None
            }
        }
    })
}

fn warn_once(msg: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn!("--schemata: {msg}");
    }
}

fn in_ranges(ranges: &[(u64, u64)], idx: u64) -> bool {
    ranges.iter().any(|&(lo, hi)| lo <= idx && idx <= hi)
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served(code: Option<i32>, error: Option<&str>) -> Served {
        Served {
            // Raw wait status: exit code in the high byte.
            status: code.map(|c| ExitStatus::from_raw(c << 8)),
            result: RunResult::default(),
            wall: Duration::ZERO,
            error: error.map(str::to_string),
            ran: Vec::new(),
            unconfirmed: false,
        }
    }

    #[test]
    fn verdicts_from_the_schema_stand_except_child_failures_and_unknown_ids() {
        assert!(usable(&served(Some(0), None)));
        assert!(usable(&served(Some(1), None)));
        assert!(usable(&served(None, None)), "a timeout is a verdict");
        assert!(
            !usable(&served(Some(3), Some("Traceback ..."))),
            "the child broke itself"
        );
        assert!(
            !usable(&served(Some(4), None)),
            "ids the template didn't collect"
        );
    }

    #[test]
    fn shebang_names_the_interpreter_only_when_it_is_a_python_path() {
        let dir = tempfile::tempdir().unwrap();
        let python = dir.path().join("python3.12");
        std::fs::write(&python, "").unwrap();
        let script = format!("#!{}\nimport pytest\n", python.display());
        assert_eq!(shebang_interpreter(script.as_bytes()), Some(python.clone()));
        let with_flag = format!("#!{} -E\n", python.display());
        assert_eq!(shebang_interpreter(with_flag.as_bytes()), Some(python));
        assert_eq!(shebang_interpreter(b"#!/usr/bin/env python3\n"), None);
        assert_eq!(shebang_interpreter(b"#!/bin/sh\n'''exec' x\n"), None);
        assert_eq!(shebang_interpreter(b"#!/no/such/python3\n"), None);
        assert_eq!(shebang_interpreter(b"import pytest\n"), None);
    }

    #[test]
    fn unsafe_ranges_are_inclusive() {
        let ranges = [(3, 5), (9, 9)];
        assert!(!in_ranges(&ranges, 2));
        assert!(in_ranges(&ranges, 3) && in_ranges(&ranges, 5) && in_ranges(&ranges, 9));
        assert!(!in_ranges(&ranges, 6) && !in_ranges(&ranges, 10));
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }

    /// A schema whose "template" is `sh -c script`, so the protocol and its
    /// failure handling run without pytest. Each schema gets its own
    /// generation, so a test never inherits a template from another.
    fn fake_schema(dir: &std::path::Path, script: &str) -> Schema {
        static NEXT: AtomicU64 = AtomicU64::new(1 << 62);
        let dir = dir.canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        let mirror =
            super::super::build_mirror(&dir.join("tests"), crate::config::IsolationMode::Copy)
                .unwrap();
        Schema {
            generation: NEXT.fetch_add(1, Ordering::Relaxed),
            mirror,
            index: HashMap::new(),
            serve_args: vec!["-c".into(), script.into()],
            python: "/bin/sh".into(),
            ready_timeout: Duration::from_secs(10),
            verified: Chunks::new(1),
        }
    }

    fn fake_runner(dir: &std::path::Path) -> SchemaRunner {
        SchemaRunner::new(PytestRunner::new(super::super::pytest::PytestConfig {
            tests: dir.join("tests"),
            timeout: Duration::from_secs(30),
            baseline_timeout: Duration::from_secs(30),
            hypothesis_seed: None,
            extra_args: Vec::new(),
            isolation: crate::config::IsolationMode::Copy,
            coverage: None,
            python: Some("/bin/false".into()),
            exe: "pytest",
            smart_order: false,
            kill_order: Arc::new(crate::kill_order::KillOrder::default()),
            kill_sink: Arc::new(Mutex::new(Vec::new())),
            record_kill_sets: false,
            kill_set_sink: Arc::new(Mutex::new(Vec::new())),
            learn_killer: false,
            key_base: dir.to_path_buf(),
            plugin: false,
            baseline_ids: None,
        }))
    }

    /// A template that answers every request with pid 0 (never a real group,
    /// so a timeout's kill is a no-op) and then `done`, after `delay`.
    fn answering(done: &str, delay: &str) -> String {
        format!(
            "echo '{{\"ready\":true,\"unsafe\":[[2,4]]}}'; \
             while read req; do echo '{{\"pid\":0}}'; sleep {delay}; echo '{done}'; done"
        )
    }

    #[test]
    fn template_reports_ready_and_unsafe_ranges_then_serves_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = fake_schema(
            tmp.path(),
            &answering(
                r#"{"status":256,"failed":["t.py::a"],"test_secs":0.5}"#,
                "0",
            ),
        );
        let mut t = Template::start(&schema).unwrap();
        assert!(t.is_unsafe(3) && !t.is_unsafe(5));
        let s = t.request(5, None, false, Duration::from_secs(10)).unwrap();
        assert_eq!(s.status.and_then(|st| st.code()), Some(1));
        assert_eq!(s.result.failures, ["t.py::a"]);
        assert!((s.result.test_secs - 0.5).abs() < f64::EPSILON);
        assert!(s.error.is_none());
    }

    #[test]
    fn template_that_refuses_or_dies_before_ready_does_not_start() {
        let tmp = tempfile::tempdir().unwrap();
        let refused = fake_schema(tmp.path(), r#"echo '{"ready":false,"error":"no fork"}'"#);
        let e = format!("{:#}", Template::start(&refused).err().unwrap());
        assert!(e.contains("template refused: no fork"), "{e}");

        let tmp = tempfile::tempdir().unwrap();
        let died = fake_schema(tmp.path(), "echo 'ImportError: boom' >&2; exit 1");
        let e = format!("{:#}", Template::start(&died).err().unwrap());
        assert!(e.contains("never became ready"), "{e}");
        assert!(e.contains("ImportError: boom"), "stderr tail is shown: {e}");
        assert!(e.contains("the template exited"), "{e}");
    }

    #[test]
    fn a_run_past_its_timeout_has_no_status() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = fake_schema(tmp.path(), &answering(r#"{"status":9}"#, "1"));
        let mut t = Template::start(&schema).unwrap();
        let s = t
            .request(1, None, false, Duration::from_millis(100))
            .unwrap();
        assert!(s.status.is_none(), "timed out");
    }

    #[test]
    fn a_template_that_exits_mid_request_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = fake_schema(
            tmp.path(),
            r#"echo '{"ready":true}'; read req; echo '{"pid":0}'; exit 0"#,
        );
        let mut t = Template::start(&schema).unwrap();
        assert!(t.request(1, None, false, Duration::from_secs(10)).is_err());
    }

    #[test]
    fn chunks_hand_out_each_slice_once_and_open_when_all_pass() {
        let c = Chunks::new(3);
        assert_eq!((c.claim(), c.claim()), (Some(0), Some(1)));
        c.finish(true);
        c.finish(true);
        assert!(!c.passed(), "slice 2 not checked yet");
        assert_eq!(c.claim(), Some(2));
        assert_eq!(c.claim(), None, "every slice handed out");
        c.finish(true);
        assert!(c.passed());

        let bad = Chunks::new(2);
        let _ = (bad.claim(), bad.claim());
        bad.finish(false);
        bad.finish(true);
        assert!(bad.failed());
        assert!(!bad.passed(), "one failed slice keeps the schema off");
    }

    #[test]
    fn verify_resumes_a_slice_past_a_test_whose_fork_crashed() {
        let check = |resumed: &str| {
            let tmp = tempfile::tempdir().unwrap();
            // Crashes (SIGSEGV) on t.py::net unless asked to resume after it.
            let script = format!(
                r#"echo '{{"ready":true}}'
while read req; do
  echo '{{"pid":0}}'
  case "$req" in
    *after*) echo '{resumed}' ;;
    *) echo '{{"status":11,"unfinished":"t.py::net"}}' ;;
  esac
done"#
            );
            let schema = fake_schema(tmp.path(), &script);
            verify(
                &mut Template::start(&schema).unwrap(),
                (0, 1),
                Duration::from_secs(10),
            )
        };
        // Each returns at all only because the slice resumed past t.py::net.
        assert!(check(r#"{"status":0}"#), "crash skipped, rest passed");
        assert!(
            check(r#"{"status":256,"failed":["t.py::a"]}"#),
            "a failure is left to kill confirmation"
        );
        assert!(
            check(r#"{"status":11,"unfinished":"t.py::net"}"#),
            "the same test crashing again ends the slice instead of looping"
        );
        assert!(
            !check(r#"{"status":768,"error":"Traceback"}"#),
            "a broken helper still fails it"
        );
    }

    #[test]
    fn a_crashed_child_is_not_a_verdict() {
        let served = |raw: i32| Served {
            status: Some(ExitStatus::from_raw(raw)),
            result: RunResult::default(),
            wall: Duration::ZERO,
            error: None,
            ran: Vec::new(),
            unconfirmed: false,
        };
        assert!(!usable(&served(libc::SIGSEGV)));
        assert!(!usable(&served(libc::SIGABRT)));
        assert!(usable(&served(1 << 8)), "exit 1: a real kill");
        assert!(usable(&served(0)));
    }

    #[test]
    fn a_kill_counts_only_if_its_tests_pass_with_no_mutant_active() {
        for (no_mutant, kill_stands) in [
            (r#"{"status":0}"#, true),
            (r#"{"status":256,"failed":["t.py::a"]}"#, false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let log = tmp.path().canonicalize().unwrap().join("requests");
            let script = format!(
                "echo '{{\"ready\":true}}'\n\
                 while read req; do echo \"$req\" >> '{}'; echo '{{\"pid\":0}}'; echo '{no_mutant}'; done",
                log.display()
            );
            let schema = fake_schema(tmp.path(), &script);
            let runner = fake_runner(&tmp.path().canonicalize().unwrap());
            let mut t = Template::start(&schema).unwrap();
            let ran = vec!["t.py::a".to_string(), "t.py::b".to_string()];
            assert_eq!(runner.confirm(&mut t, &ran), kill_stands);
            assert_eq!(runner.confirm(&mut t, &ran), kill_stands, "cached");
            let requests = std::fs::read_to_string(&log).unwrap();
            assert_eq!(
                requests.lines().count(),
                1,
                "one fork per test list: {requests}"
            );
            assert!(
                requests.contains(r#""idx":0"#) && requests.contains(r#"["t.py::a","t.py::b"]"#),
                "the same tests, no mutant active: {requests}"
            );
        }
    }

    #[test]
    fn verify_fails_only_when_the_helper_itself_breaks() {
        let check = |done: &str, delay: &str, timeout: Duration| {
            let tmp = tempfile::tempdir().unwrap();
            let schema = fake_schema(tmp.path(), &answering(done, delay));
            verify(&mut Template::start(&schema).unwrap(), (0, 1), timeout)
        };
        let secs = Duration::from_secs(10);
        assert!(check(r#"{"status":0}"#, "0", secs));
        // Failing and hung tests are left to each kill's confirmation.
        assert!(check(r#"{"status":256,"failed":["t.py::a"]}"#, "0", secs));
        assert!(check(r#"{"status":0}"#, "1", Duration::from_millis(100)));
        assert!(!check(r#"{"status":768,"error":"Traceback"}"#, "0", secs));
        assert!(!check("not json", "0", secs));
    }

    #[test]
    fn a_template_that_fails_to_start_is_not_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let starts = tmp.path().canonicalize().unwrap().join("starts");
        let schema = fake_schema(
            tmp.path(),
            &format!("echo x >> '{}'; exit 1", starts.display()),
        );
        for _ in 0..2 {
            assert_eq!(with_template(&schema, |_| Ok(Some(1))), None);
        }
        assert_eq!(std::fs::read_to_string(&starts).unwrap(), "x\n");
    }

    #[test]
    fn a_template_whose_request_fails_is_dropped_for_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = fake_schema(tmp.path(), &answering(r#"{"status":0}"#, "0"));
        assert_eq!(with_template(&schema, |_| Ok(Some(1))), Some(1));
        assert_eq!(
            with_template(&schema, |_| -> Result<Option<i32>> { bail!("wedged") }),
            None
        );
        let mut called = false;
        let out = with_template(&schema, |_| {
            called = true;
            Ok(Some(1))
        });
        assert_eq!(out, None);
        assert!(!called, "a dead template is not asked again");
    }

    #[test]
    fn a_schema_that_fails_to_build_leaves_every_mutant_to_the_spawn_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        let runner = fake_runner(&dir);
        runner.prepare(&[]).unwrap();
        assert!(runner.schema().is_none());
    }

    #[test]
    fn helper_failure_carries_its_exit_status_and_stderr() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "cat >/dev/null; echo 'bad input' >&2; exit 3"]);
        let e = format!("{:#}", run_with_stdin(cmd, b"[]").err().unwrap());
        assert!(
            e.contains("exit status: 3") && e.contains("bad input"),
            "{e}"
        );

        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "cat"]);
        assert_eq!(run_with_stdin(cmd, b"echo").unwrap(), b"echo");
    }
}
