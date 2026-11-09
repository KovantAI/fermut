//! Schemata mode for the pytest runner (on by default on Unix; `--no-schemata`),
//! and opt-in for rstest and unittest (`--schemata`).
//!
//! Instead of splicing one mutant into a mirror and starting pytest per
//! mutant, every function-body mutant is compiled into one *schema* up front:
//! each mutated function gets a switch on an active-mutant index (see
//! `python/pytest_fermut/_fermut_schema.py`). One *template* — a pytest
//! process that imported and collected the suite once — serves every rayon
//! worker: a worker runs a mutant by asking it to fork a child with that index
//! active, and the template runs those children side by side. The
//! child runs the same selected tests a spawned pytest would, in the same
//! order, and the verdict goes through the same classification as the spawn
//! path.
//!
//! Anything the schema can't serve safely runs on the wrapped runner
//! ([`SchemaInner`]) exactly as without `--schemata`: mutants outside function
//! bodies, ones that would warn at compile time, functions that already ran
//! inside the template (their results may have been stored before the fork),
//! a template that failed to start or died, a child that broke itself, and
//! a run that timed out.
//! The baseline is always the cold, spawn-path run.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::pytest::{run_timing, PytestRunner};
use super::pytest_result::RunResult;
use super::python::pytest_interpreter;
use super::unittest::UnittestRunner;
use super::{configure_mirror_cmd, precompile, BaselineStatus, Mirror, Runner};
use crate::config::IsolationMode;
use crate::mutator::Mutant;
use crate::report::{MutantOutcome, Timing};
use crate::sync::lock_recover;

/// The helper script, written next to the schema mirror.
const SCRIPT_SOURCE: &str = include_str!("../../python/pytest_fermut/_fermut_schema.py");

/// Waiting for a template's `{"pid"}` after a request: a fork is instant, so
/// missing this means the template is wedged.
const FORK_TIMEOUT: Duration = Duration::from_secs(30);

/// Waiting for the result past the run's timeout: the template enforces the
/// timeout itself and only has to kill and reap the child. Missing this means
/// the template is wedged.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// What the schema needs from the runner it wraps: how to start its template,
/// what a mutant's fork runs, and how a served run becomes a verdict.
pub(crate) trait SchemaInner: Runner {
    fn schema_env(&self) -> SchemaEnv;
    /// The per-mutant timeout, read per request: set after the baseline.
    fn timeout(&self) -> Duration;
    /// The helper's arguments after its script path: the command, the
    /// mirror root, `schema_json`, then the runner's own.
    fn serve_args(&self, mirror: &Mirror, schema_json: &Path) -> Vec<OsString>;
    /// The tests a fork runs for `mutant`, in order (`None`: all of them),
    /// as the spawn path would select them; `only` narrows to one.
    fn selection(&self, mutant: &Arc<Mutant>, only: Option<&str>) -> Option<Vec<String>>;
    /// Whether a run that ended with `status` killed the mutant. A kill must
    /// be confirmed with no mutant active before it stands.
    fn killed(&self, status: ExitStatus) -> bool;
    /// The outcome the spawn path would have given for `served`.
    fn served_outcome(&self, mutant: &Arc<Mutant>, served: &Served, record: bool) -> MutantOutcome;
    /// Run every selected test even past a failure.
    fn record_all(&self) -> bool {
        false
    }
}

/// How the template runs, fixed when the schema is built.
pub(crate) struct SchemaEnv {
    tests: PathBuf,
    isolation: IsolationMode,
    python: PathBuf,
    private_home: bool,
    mem_cap: Option<u64>,
    baseline_timeout: Duration,
    /// The template runs on rstest's vendored pytest.
    rstest: bool,
}

impl SchemaInner for PytestRunner {
    fn schema_env(&self) -> SchemaEnv {
        SchemaEnv {
            tests: self.tests.clone(),
            isolation: self.isolation,
            python: pytest_interpreter(self.python.as_deref()),
            private_home: self.private_home,
            mem_cap: self.mem_cap,
            baseline_timeout: self.baseline_timeout,
            rstest: self.exe == "rstest",
        }
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn serve_args(&self, mirror: &Mirror, schema_json: &Path) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "serve".into(),
            mirror.root.clone().into(),
            schema_json.into(),
            "--tb=no".into(),
            "-q".into(),
        ];
        if let Some(seed) = self.hypothesis_seed {
            args.push(format!("--hypothesis-seed={seed}").into());
        }
        args.extend(self.extra_args.iter().map(Into::into));
        args.push(mirror.tests.clone().into());
        args
    }

    fn selection(&self, mutant: &Arc<Mutant>, only: Option<&str>) -> Option<Vec<String>> {
        let (key_file, key_op) = self.kill_key(mutant);
        self.coverage_ids(mutant, only).map(|ids| {
            if self.smart_order {
                self.kill_order.order(&ids, &key_file, key_op)
            } else {
                ids
            }
        })
    }

    /// A failing test (1) or one interrupting the run (2).
    fn killed(&self, status: ExitStatus) -> bool {
        matches!(status.code(), Some(1 | 2))
    }

    fn served_outcome(&self, mutant: &Arc<Mutant>, served: &Served, record: bool) -> MutantOutcome {
        let (key_file, key_op) = self.kill_key(mutant);
        let waited = (served.status, None);
        if record && self.record_kill_sets {
            self.record_kill_set(
                mutant,
                &waited,
                Some(&served.result),
                false,
                &key_file,
                key_op,
            );
        }
        self.classify_run(
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

    fn record_all(&self) -> bool {
        self.record_kill_sets
    }
}

/// unittest never narrows per mutant: each fork runs the whole suite,
/// failfast, as `python -m unittest discover -f` does.
impl SchemaInner for UnittestRunner {
    fn schema_env(&self) -> SchemaEnv {
        SchemaEnv {
            tests: self.tests.clone(),
            isolation: self.isolation,
            python: self.python.clone(),
            private_home: self.private_home,
            mem_cap: self.mem_cap,
            baseline_timeout: self.baseline_timeout,
            rstest: false,
        }
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn serve_args(&self, mirror: &Mirror, schema_json: &Path) -> Vec<OsString> {
        vec![
            "serve-unittest".into(),
            mirror.root.clone().into(),
            schema_json.into(),
            mirror.tests.clone().into(),
            self.pattern.clone().into(),
        ]
    }

    fn selection(&self, _mutant: &Arc<Mutant>, only: Option<&str>) -> Option<Vec<String>> {
        only.map(|k| vec![k.to_string()])
    }

    /// A failing test (1), or a KeyboardInterrupt the interpreter died of.
    fn killed(&self, status: ExitStatus) -> bool {
        status.code() == Some(1) || status.signal() == Some(libc::SIGINT)
    }

    fn served_outcome(&self, mutant: &Arc<Mutant>, served: &Served, _: bool) -> MutantOutcome {
        let outcome = match served.status {
            None => MutantOutcome::timed_out_in(mutant.clone(), served.result.unfinished.clone()),
            Some(status) => super::unittest::verdict(mutant, status.code()),
        };
        outcome.with_timing(Timing {
            duration_ms: u64::try_from(served.wall.as_millis()).unwrap_or(u64::MAX),
            test_ms: None,
        })
    }
}

pub struct SchemaRunner<R> {
    inner: R,
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
    /// Of `served`, the timeouts: hangs the no-mutant confirmation pinned on
    /// the mutant, which no longer pay their timeout again on the spawn path.
    hangs: AtomicU64,
    /// Audit (see [`AUDIT_RATE`]): how many survivors and timeouts were re-run
    /// the normal way, and how many of those the normal run disagreed with.
    audited: AtomicU64,
    disagreed: AtomicU64,
    first_audited: AtomicBool,
}

/// Fraction of survivors and timeouts from the schema re-run the normal way.
/// Confirmation catches a fork-only failure or hang in tests that also fail
/// or hang with no mutant active; this samples for what it can't see —
/// something in a fork making a test pass that a fresh process fails, or
/// hanging only on a path the mutant takes. Picked by mutant id, so the same
/// ones every run, plus the first so even a small run checks one.
const AUDIT_RATE: f64 = 0.05;
const AUDIT_SEED: u64 = 0x5c4e_a0d1;

/// Tells the template to run on rstest's vendored pytest
/// (`_fermut_schema.py`'s `RSTEST_ENV`).
const RSTEST_ENV: &str = "FERMUT_RSTEST";

/// The built schema: one read-only mirror shared by every template.
struct Schema {
    /// Shared with the template, which maps node ids into and out of it.
    mirror: Arc<Mirror>,
    /// Mutant id → its index in the schema. Absent ids run on the spawn path.
    index: HashMap<String, u64>,
    /// Template command line after the interpreter: script, `serve`, args.
    serve_args: Vec<std::ffi::OsString>,
    python: PathBuf,
    /// The runner is rstest: the template runs on rstest's vendored pytest.
    rstest: bool,
    /// Per-mutant memory cap each forked child sets on itself.
    mem_cap: Option<u64>,
    /// The template runs with a private `HOME` and gives each fork its own.
    private_home: bool,
    /// How long a template may take to import and collect the suite; also
    /// bounds the verification run.
    ready_timeout: Duration,
    /// Whether the suite passes on the schema with no mutant active (see
    /// [`verify`]), checked one slice per worker.
    verified: Chunks,
    /// The one template every worker forks from; started by the first worker
    /// to need it (see [`with_template`]).
    template: OnceLock<Option<Template>>,
    template_starting: AtomicBool,
    template_dead: AtomicBool,
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

impl<R: SchemaInner> SchemaRunner<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            mutants: Mutex::new(Vec::new()),
            schema: OnceLock::new(),
            building: AtomicBool::new(false),
            confirmed: Mutex::new(HashMap::new()),
            served: AtomicU64::new(0),
            spawned: AtomicU64::new(0),
            hangs: AtomicU64::new(0),
            audited: AtomicU64::new(0),
            disagreed: AtomicU64::new(0),
            first_audited: AtomicBool::new(false),
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
        let workers = rayon::current_num_threads();
        let built = match self.build_schema(&mutants) {
            Ok(schema) if !worth_a_template(schema.index.len(), workers) => {
                info!(
                    "schemata: {} mutant(s) in the schema for {workers} workers — not worth \
                     a template; running them normally",
                    schema.index.len()
                );
                None
            }
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
        let env = self.inner.schema_env();
        let mirror = Mirror::build(&env.tests, env.isolation)?;
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
        let python = env.python;
        let mut cmd = Command::new(&python);
        cmd.arg(&script).arg("schematize").arg(&mirror.root);
        configure_mirror_cmd(&mut cmd, &mirror, env.private_home)?;
        let out = run_with_stdin(cmd, &serde_json::to_vec(&input)?)?;
        let parsed: SchematizeOutput =
            serde_json::from_slice(&out).context("parsing the schematize output")?;
        // After the rewrite: every Python subprocess a test spawns then skips
        // recompiling the (much larger) schema source.
        precompile(&mirror.root, &python);

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
        let mut serve_args = vec![script.into()];
        serve_args.extend(self.inner.serve_args(&mirror, &schema_json));
        Ok(Schema {
            mirror: Arc::new(mirror),
            index,
            serve_args,
            python,
            rstest: env.rstest,
            mem_cap: env.mem_cap,
            private_home: env.private_home,
            ready_timeout: env.baseline_timeout,
            // One slice per worker: built inside the run's rayon pool.
            verified: Chunks::new(rayon::current_num_threads()),
            template: OnceLock::new(),
            template_starting: AtomicBool::new(false),
            template_dead: AtomicBool::new(false),
        })
    }

    /// Run `mutant` in this worker's template. `None` → run it on the spawn
    /// path instead (not in the schema, unsafe, or no usable template).
    fn serve(&self, mutant: &Arc<Mutant>, only: Option<&str>, all: bool) -> Option<Served> {
        let schema = self.schema()?;
        let idx = *schema.index.get(&mutant.id)?;
        let ids = self.inner.selection(mutant, only);
        if schema.verified.failed() {
            return None;
        }
        with_template(schema, |template| {
            // Until every slice of the suite has passed with no mutant active,
            // help check them, then run this mutant normally: no worker waits
            // on a slice another one is checking.
            if !schema.verified.passed() {
                // Check slices until none is left unclaimed: other workers
                // claim theirs side by side, and a small run that keeps few
                // workers busy still gets through all of them.
                while let Some(k) = schema.verified.claim() {
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
            let mut served = template.request(idx, ids.as_deref(), all, self.inner.timeout())?;
            // A kill or a hang must be the mutant's doing.
            if served.status.is_some_and(|s| self.inner.killed(s)) || hung_in_a_test(&served) {
                served.unconfirmed = !self.confirm(template, &served.ran);
            }
            Ok(Some(served))
        })
    }

    /// Whether a kill or a hang is the mutant's: the same tests, in the same
    /// order, pass in a fork with no mutant active. For a hang, that is what
    /// spares it a second full timeout on the spawn path: a fork has already
    /// imported and collected, so where it ran out of time a cold run, given
    /// the same budget, would too. A test can fail or hang in a fork alone, from
    /// state another test module left behind while the template collected the
    /// suite (agent-library: a manifest scan at import emptied a registry that
    /// only a test's teardown restores) — a cold run never imports that
    /// module, so the kill would be false. Cached per test list: most kills
    /// come from the first selected test.
    fn confirm(&self, template: &Template, ran: &[String]) -> bool {
        if let Some(&ok) = lock_recover(&self.confirmed).get(ran) {
            return ok;
        }
        let ok = matches!(
            template.request(0, Some(ran), false, self.inner.timeout()),
            Ok(s) if s.error.is_none() && s.status.is_some_and(|st| st.success())
        );
        if !ok {
            warn_unconfirmed(ran);
        }
        lock_recover(&self.confirmed).insert(ran.to_vec(), ok);
        ok
    }

    fn audit_pick(&self, id: &str) -> bool {
        !self.first_audited.swap(true, Ordering::Relaxed)
            || crate::filter::sample::score(AUDIT_SEED, id) < AUDIT_RATE
    }

    /// Re-run a survivor or a timeout from the schema the normal way; that
    /// run's verdict stands. A survivor the normal run detects, or a timeout
    /// it lets survive, means a fork made a test pass that a fresh process
    /// fails, or hung where a fresh process passes — worth a report. A
    /// timeout the normal run kills isn't: a mutant that dies near the budget
    /// (a runaway allocation hitting the memory cap) lands on either side
    /// run to run, and both count as detected.
    fn audit_normal(&self, mutant: &Arc<Mutant>, served: Served) -> Result<MutantOutcome> {
        self.audited.fetch_add(1, Ordering::Relaxed);
        let fork = if served.status.is_none() {
            "timeout"
        } else {
            "survived"
        };
        drop(served);
        let normal = self.inner.run(mutant)?;
        let detected = matches!(
            normal,
            MutantOutcome::Killed { .. } | MutantOutcome::TimedOut { .. }
        );
        let survived = matches!(normal, MutantOutcome::Survived { .. });
        if (fork == "survived" && detected) || (fork == "timeout" && survived) {
            self.disagreed.fetch_add(1, Ordering::Relaxed);
            warn!(
                "--schemata: {} is {fork} in a fork but {} in a normal run; using the normal run. \
                 Please report this (and compare with --no-schemata).",
                mutant.id,
                normal.status_label()
            );
        }
        Ok(normal)
    }
}

impl<R> Drop for SchemaRunner<R> {
    fn drop(&mut self) {
        let (served, spawned) = (*self.served.get_mut(), *self.spawned.get_mut());
        if served + spawned > 0 {
            let hangs = *self.hangs.get_mut();
            info!(
                "schemata: {served} mutants run from the schema ({hangs} timed out), {spawned} run normally"
            );
        }
        let (audited, disagreed) = (*self.audited.get_mut(), *self.disagreed.get_mut());
        if audited > 0 {
            info!(
                "schemata: audit re-ran {audited} survivor(s)/timeout(s) the normal way; \
                 {} agreed, {disagreed} didn't",
                audited - disagreed
            );
        }
    }
}

/// Run slice `chunk = (k, n)` of the suite on the schema with no mutant
/// active (every slice, across the workers, covers the suite), every test even
/// past a failure. Tests that fail, crash or hang here don't turn schemata
/// off: a kill from the schema only stands once the same tests pass with no
/// mutant active ([`SchemaRunner::confirm`]), and neither does a timeout, so
/// they just cost those mutants a normal run. This reports them, and resumes a slice past a test
/// that took its child down (a crash: macOS DNS, proxy and Kerberos frameworks
/// aren't fork-safe; or a hang). Only a broken helper (a traceback) fails it.
fn verify(template: &Template, chunk: (usize, usize), timeout: Duration) -> bool {
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

/// Whether a schema this small pays for its template. Until the template has
/// imported and collected the suite, workers run mutants the normal way, so
/// a run with only a couple of mutants per worker finishes before it serves
/// any: the template and its no-mutant check would be pure overhead (a
/// `--diff-only` PR touching a few lines).
fn worth_a_template(in_schema: usize, workers: usize) -> bool {
    in_schema >= 2 * workers
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
    // A timeout is one only inside a test whose run confirmation reproduced
    // with no mutant active; a hang before any test began (collection, the
    // helper) has nothing to confirm, so the spawn path decides.
    (served.status.is_some() || hung_in_a_test(served))
        && !served.unconfirmed
        && !served.status.is_some_and(crash_signal)
        && served.status.and_then(|s| s.code()) != Some(4)
}

/// Timed out (the child was killed) while running a test.
fn hung_in_a_test(served: &Served) -> bool {
    served.status.is_none() && served.error.is_none() && served.result.unfinished.is_some()
}

fn warn_unconfirmed(ran: &[String]) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "--schemata: {} fails or hangs in a forked run even with no mutant active; mutants it kills or hangs run normally",
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

impl<R: SchemaInner> Runner for SchemaRunner<R> {
    fn prepare(&self, mutants: &[Arc<Mutant>]) -> Result<()> {
        *lock_recover(&self.mutants) = mutants.to_vec();
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) {
        self.inner.set_timeout(timeout);
    }

    fn set_memory_cap(&mut self, bytes: u64) {
        self.inner.set_memory_cap(bytes);
    }

    fn run(&self, mutant: &Arc<Mutant>) -> Result<MutantOutcome> {
        let all = self.inner.record_all();
        match self.serve(mutant, None, all) {
            Some(served) if usable(&served) => {
                self.served.fetch_add(1, Ordering::Relaxed);
                let timed_out = served.status.is_none();
                if timed_out {
                    self.hangs.fetch_add(1, Ordering::Relaxed);
                }
                let survived = served.status.and_then(|s| s.code()) == Some(0);
                if (survived || timed_out) && self.audit_pick(&mutant.id) {
                    return self.audit_normal(mutant, served);
                }
                Ok(self.inner.served_outcome(mutant, &served, true))
            }
            _ => {
                self.spawned.fetch_add(1, Ordering::Relaxed);
                self.inner.run(mutant)
            }
        }
    }

    fn audit(&self, mutant: &Arc<Mutant>, killer: &str) -> Result<Option<MutantOutcome>> {
        match self.serve(mutant, Some(killer), false) {
            Some(served) if usable(&served) => {
                Ok(Some(self.inner.served_outcome(mutant, &served, false)))
            }
            _ => self.inner.audit(mutant, killer),
        }
    }

    fn baseline(&self) -> Result<BaselineStatus> {
        self.inner.baseline()
    }

    fn baseline_deselection(&self) -> Option<super::Deselection> {
        self.inner.baseline_deselection()
    }

    fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
        self.inner.take_kill_records()
    }

    fn take_kill_sets(&self) -> Vec<super::kill_sets::KillSetRecord> {
        self.inner.take_kill_sets()
    }
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
pub(crate) struct Served {
    /// `None` when it timed out (the child was killed).
    status: Option<ExitStatus>,
    result: RunResult,
    wall: Duration,
    /// Traceback when the child itself broke rather than a test failing.
    error: Option<String>,
    /// The tests the child ran, in order.
    ran: Vec<String>,
    /// A kill or hang that the same tests reproduce with no mutant active:
    /// not the mutant's doing (see [`SchemaRunner::confirm`]).
    unconfirmed: bool,
}

#[derive(Deserialize)]
struct Ready {
    ready: bool,
    #[serde(default, rename = "unsafe")]
    unsafe_ranges: Vec<(u64, u64)>,
    #[serde(default)]
    error: Option<String>,
    /// Third-party daemon threads the template allowed (name and code).
    #[serde(default)]
    threads: Vec<String>,
}

/// Read only to keep replies in step: the template does any killing.
#[derive(Deserialize)]
struct Pid {
    pid: u32,
}

#[derive(Deserialize)]
struct Done {
    status: i32,
    #[serde(default)]
    timed_out: bool,
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

/// The template process every worker shares, and the protocol over its
/// stdio: requests carry an id, the template forks each at once and answers
/// with that id, so many children run side by side under one parent that
/// imported and collected the suite once.
struct Template {
    child: Mutex<Child>,
    /// Memory cap the watchdog holds each forked child to (macOS; on Linux
    /// the child sets its own rlimit).
    mem_cap: Option<u64>,
    stdin: Mutex<ChildStdin>,
    /// Requests waiting on an answer, by id; the reader thread routes lines to
    /// them and drops them all when the template exits.
    waiting: Arc<Mutex<HashMap<u64, mpsc::Sender<String>>>>,
    next_id: AtomicU64,
    /// Index ranges of functions that ran inside the template before forking.
    unsafe_ranges: Vec<(u64, u64)>,
    /// The schema's mirror; node ids are mapped in and out of it.
    mirror: Arc<Mirror>,
}

impl Template {
    fn start(schema: &Schema) -> Result<Self> {
        let mut cmd = Command::new(&schema.python);
        cmd.args(&schema.serve_args);
        configure_mirror_cmd(&mut cmd, &schema.mirror, schema.private_home)?;
        cmd.env_remove("FM_ACTIVE");
        if schema.rstest {
            cmd.env(RSTEST_ENV, "1");
        }
        // macOS unified logging isn't fork-safe: a forked child that reads a
        // system preference (PDFium's CoreGraphics, psycopg's Kerberos) dies
        // in `_os_log_preferences_refresh`. Off from the template's start, the
        // children never reach it. A value the user exports wins.
        if cfg!(target_os = "macos") && std::env::var_os("OS_ACTIVITY_MODE").is_none() {
            cmd.env("OS_ACTIVITY_MODE", "disable");
        }
        // The template stays uncapped; each forked child caps itself.
        match schema.mem_cap {
            Some(bytes) => cmd.env(super::memory::CAP_ENV, bytes.to_string()),
            None => cmd.env_remove(super::memory::CAP_ENV),
        };
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
        let waiting: Arc<Mutex<HashMap<u64, mpsc::Sender<String>>>> = Arc::default();
        let (ready_tx, ready_rx) = mpsc::channel();
        let routes = Arc::clone(&waiting);
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            if let Some(Ok(line)) = lines.next() {
                let _ = ready_tx.send(line);
            }
            for line in lines {
                let Ok(line) = line else { break };
                let id = serde_json::from_str::<Routed>(&line)
                    .ok()
                    .and_then(|r| r.id);
                if let Some(tx) = id.and_then(|id| lock_recover(&routes).get(&id).cloned()) {
                    let _ = tx.send(line);
                }
            }
            lock_recover(&routes).clear(); // every waiter sees the exit
        });
        let line = recv(&ready_rx, schema.ready_timeout).with_context(|| {
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
        if !ready.threads.is_empty() {
            info!(
                "schemata: forking beside third-party daemon thread(s): {}",
                ready.threads.join(", ")
            );
        }
        debug!(unsafe_ranges = ?ready.unsafe_ranges, "template ready");
        Ok(Self {
            child: Mutex::new(child),
            mem_cap: schema.mem_cap,
            stdin: Mutex::new(stdin),
            waiting,
            next_id: AtomicU64::new(0),
            unsafe_ranges: ready.unsafe_ranges,
            mirror: Arc::clone(&schema.mirror),
        })
    }

    fn is_unsafe(&self, idx: u64) -> bool {
        in_ranges(&self.unsafe_ranges, idx)
    }

    fn request(
        &self,
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

    fn send(&self, req: &serde_json::Value, timeout: Duration) -> Result<Served> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock_recover(&self.waiting).insert(id, tx);
        let answer = self.exchange(id, req, &rx, timeout);
        lock_recover(&self.waiting).remove(&id);
        answer
    }

    fn exchange(
        &self,
        id: u64,
        req: &serde_json::Value,
        rx: &Receiver<String>,
        timeout: Duration,
    ) -> Result<Served> {
        let mut req = req.clone();
        req["id"] = id.into();
        // The template kills a child past `timeout` itself: it alone knows the
        // child isn't reaped yet, so its group id can't have been recycled.
        req["timeout"] = timeout.as_secs_f64().into();
        // Ids as pytest names them in the mirror (parametrize ids can embed
        // the project path), and results mapped back, as on the spawn path.
        let into_mirror = |v: &serde_json::Value| -> Option<serde_json::Value> {
            v.as_str().map(|id| self.mirror.id_into_mirror(id).into())
        };
        if let Some(ids) = req["ids"].as_array_mut() {
            for v in ids.iter_mut() {
                if let Some(mapped) = into_mirror(v) {
                    *v = mapped;
                }
            }
        }
        if let Some(mapped) = into_mirror(&req["after"]) {
            req["after"] = mapped;
        }
        {
            let mut stdin = lock_recover(&self.stdin);
            writeln!(stdin, "{req}").context("sending a request")?;
            stdin.flush().context("sending a request")?;
        }
        let pid: Pid =
            serde_json::from_str(&recv(rx, FORK_TIMEOUT)?).context("parsing the fork reply")?;
        let started = Instant::now();
        let watch = super::memory::watch(pid.pid, self.mem_cap);
        let line = recv(rx, timeout + REAP_GRACE)?;
        drop(watch);
        let wall = started.elapsed();
        let done: Done = serde_json::from_str(&line).context("parsing the run result")?;
        let from_mirror = |id: String| self.mirror.text_from_mirror(&id);
        Ok(Served {
            status: (!done.timed_out).then(|| ExitStatus::from_raw(done.status)),
            result: RunResult {
                failures: done.failed.into_iter().map(from_mirror).collect(),
                unfinished: done.unfinished.map(from_mirror),
                test_secs: done.test_secs,
                ..RunResult::default()
            },
            wall,
            error: done.error,
            ran: done.ran.into_iter().map(from_mirror).collect(),
            unconfirmed: false,
        })
    }
}

/// Just enough of a template line to route it.
#[derive(Deserialize)]
struct Routed {
    id: Option<u64>,
}

fn recv(rx: &Receiver<String>, timeout: Duration) -> Result<String> {
    match rx.recv_timeout(timeout) {
        Ok(line) => Ok(line),
        Err(RecvTimeoutError::Timeout) => bail!("no answer within {timeout:?}"),
        Err(RecvTimeoutError::Disconnected) => bail!("the template exited"),
    }
}

impl Drop for Template {
    fn drop(&mut self) {
        // EOF on stdin ends it cleanly; the kill covers a wedged one.
        let child = self.child.get_mut().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Run `f` on the shared template, which the first worker to ask starts while
/// the others run their mutants normally instead of waiting on it. `None` →
/// run the mutant on the spawn path. A template that failed to start, or whose
/// request failed (it may be wedged), is off for the rest of the run: one bad
/// environment costs one warning, not a retry per mutant.
fn with_template<T>(schema: &Schema, f: impl FnOnce(&Template) -> Result<Option<T>>) -> Option<T> {
    if schema.template_dead.load(Ordering::Acquire) {
        return None;
    }
    let template = match schema.template.get() {
        Some(t) => t.as_ref()?,
        None => {
            if schema.template_starting.swap(true, Ordering::AcqRel) {
                return None;
            }
            let started = Template::start(schema)
                .map_err(|e| {
                    warn_once(&format!(
                        "the template couldn't start, running every mutant normally: {e:#}"
                    ));
                })
                .ok();
            let _ = schema.template.set(started);
            schema.template.get()?.as_ref()?
        }
    };
    match f(template) {
        Ok(out) => out,
        Err(e) => {
            warn_once(&format!(
                "the template stopped answering, running the remaining mutants normally: {e:#}"
            ));
            schema.template_dead.store(true, Ordering::Release);
            None
        }
    }
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
        assert!(
            !usable(&served(None, None)),
            "a timeout before any test began: nothing to confirm it against"
        );
        let mut hung = served(None, None);
        hung.result.unfinished = Some("t.py::loops".into());
        assert!(usable(&hung), "a confirmed hang in a test");
        hung.unconfirmed = true;
        assert!(
            !usable(&hung),
            "a hang the same tests reproduce with no mutant active"
        );
        assert!(
            !usable(&served(Some(3), Some("Traceback ..."))),
            "the child broke itself"
        );
        assert!(
            !usable(&served(Some(3), Some("Traceback ..."))),
            "still unusable once the first failure was warned about"
        );
        assert!(
            !usable(&served(Some(4), None)),
            "ids the template didn't collect"
        );
    }

    fn unittest_runner() -> UnittestRunner {
        UnittestRunner::new(
            "tests".into(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            IsolationMode::Copy,
            true,
            "python".into(),
            None,
        )
    }

    fn some_mutant() -> Arc<Mutant> {
        Arc::new(Mutant {
            id: "src/m.py@1:op:a->b".into(),
            file: "m.py".into(),
            operator: crate::mutator::Operator::ArithOpSwap,
            range: ruff_text_size::TextRange::default(),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
            site: None,
        })
    }

    #[test]
    fn a_unittest_fork_scores_like_python_m_unittest() {
        let (r, m) = (unittest_runner(), some_mutant());
        let label = |s: &Served| r.served_outcome(&m, s, true).status_label();
        assert_eq!(label(&served(Some(0), None)), "survived");
        assert_eq!(label(&served(Some(1), None)), "killed");
        assert_eq!(label(&served(Some(5), None)), "error", "no tests ran");
        let mut sigint = served(None, None);
        sigint.status = Some(ExitStatus::from_raw(libc::SIGINT));
        assert_eq!(label(&sigint), "killed", "died of a KeyboardInterrupt");
        let mut hung = served(None, None);
        hung.result.unfinished = Some("t.T.test_loops".into());
        let o = r.served_outcome(&m, &hung, true);
        assert_eq!(o.hung_in(), Some("t.T.test_loops"));
    }

    #[test]
    fn kills_that_need_confirming_follow_each_runners_exit_codes() {
        let code = |c: i32| ExitStatus::from_raw(c << 8);
        let sigint = ExitStatus::from_raw(libc::SIGINT);
        let (ut, schema) = (unittest_runner(), fake_runner(Path::new("/nonexistent")));
        let py = &schema.inner;
        assert!(ut.killed(code(1)) && ut.killed(sigint));
        assert!(!ut.killed(code(0)) && !ut.killed(code(2)) && !ut.killed(code(5)));
        assert!(py.killed(code(1)) && py.killed(code(2)));
        assert!(!py.killed(code(0)) && !py.killed(code(4)) && !py.killed(sigint));
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
    /// failure handling run without pytest. Every script gets [`FAKE`]: in its
    /// request loop `$id` is the request's id and `reply '<json object>'`
    /// answers it.
    fn fake_schema(dir: &std::path::Path, script: &str) -> Schema {
        let dir = dir.canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        let mirror =
            super::super::build_mirror(&dir.join("tests"), crate::config::IsolationMode::Copy)
                .unwrap();
        Schema {
            mirror: Arc::new(mirror),
            index: HashMap::new(),
            serve_args: vec!["-c".into(), format!("{FAKE}{script}").into()],
            python: "/bin/sh".into(),
            rstest: false,
            mem_cap: None,
            private_home: true,
            ready_timeout: Duration::from_secs(10),
            verified: Chunks::new(1),
            template: OnceLock::new(),
            template_starting: AtomicBool::new(false),
            template_dead: AtomicBool::new(false),
        }
    }

    const FAKE: &str = r#"reply() { printf '{"id":%s,%s\n' "$id" "${1#\{}"; }
idof() { id=$(printf %s "$1" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'); }
"#;

    #[test]
    fn the_template_answers_concurrent_requests_out_of_order() {
        let tmp = tempfile::tempdir().unwrap();
        // Each request answers after `idx` seconds, in its own background job.
        let schema = fake_schema(
            tmp.path(),
            r#"echo '{"ready":true}'
while read req; do
  idof "$req"; reply '{"pid":0}'
  idx=$(printf %s "$req" | sed -n 's/.*"idx":\([0-9]*\).*/\1/p')
  ( sleep "$idx"; reply "{\"status\":$((idx << 8))}" ) &
done"#,
        );
        let t = Template::start(&schema).unwrap();
        let started = Instant::now();
        let (slow, fast) = std::thread::scope(|s| {
            let slow = s.spawn(|| t.request(2, None, false, Duration::from_secs(10)));
            std::thread::sleep(Duration::from_millis(200));
            let fast = s.spawn(|| t.request(0, None, false, Duration::from_secs(10)));
            (slow.join().unwrap().unwrap(), fast.join().unwrap().unwrap())
        });
        assert_eq!(
            slow.status.and_then(|s| s.code()),
            Some(2),
            "its own answer"
        );
        assert_eq!(
            fast.status.and_then(|s| s.code()),
            Some(0),
            "its own answer"
        );
        assert!(
            fast.wall < Duration::from_secs(1),
            "not queued behind the slow one"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "ran side by side"
        );
    }

    fn fake_runner(dir: &std::path::Path) -> SchemaRunner<PytestRunner> {
        SchemaRunner::new(PytestRunner::new(super::super::pytest::PytestConfig {
            tests: dir.join("tests"),
            timeout: Duration::from_secs(30),
            baseline_timeout: Duration::from_secs(30),
            private_home: true,
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

    /// A template that answers every request with pid 0 and then `done`,
    /// after `delay`.
    fn answering(done: &str, delay: &str) -> String {
        format!(
            "echo '{{\"ready\":true,\"unsafe\":[[2,4]]}}'; \
             while read req; do idof \"$req\"; reply '{{\"pid\":0}}'; sleep {delay}; reply '{done}'; done"
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
        let t = Template::start(&schema).unwrap();
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
    fn a_run_the_template_timed_out_has_no_status() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = fake_schema(
            tmp.path(),
            &answering(r#"{"status":9,"timed_out":true}"#, "0"),
        );
        let t = Template::start(&schema).unwrap();
        let s = t.request(1, None, false, Duration::from_secs(10)).unwrap();
        assert!(s.status.is_none(), "timed out");
    }

    #[test]
    fn requests_carry_the_timeout_and_ids_map_into_and_out_of_the_mirror() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let log = dir.join("req.json");
        let schema = fake_schema(
            &dir,
            &format!(
                r#"echo '{{"ready":true}}'; read req; echo "$req" > '{}'; idof "$req"; reply '{{"pid":0}}'; \
                   reply "{{\"status\":256,\"failed\":[\"tests/t.py::t[$(pwd -P)/x]\"]}}""#,
                log.display()
            ),
        );
        let t = Template::start(&schema).unwrap();
        let root = schema.mirror.root.display().to_string();
        let id = format!("tests/t.py::t[{}/x]", dir.display());
        let s = t
            .request(
                7,
                Some(std::slice::from_ref(&id)),
                false,
                Duration::from_millis(1500),
            )
            .unwrap();
        let req: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(req["timeout"], 1.5);
        assert_eq!(req["ids"][0], format!("tests/t.py::t[{root}/x]"));
        assert_eq!(s.result.failures, [id], "mapped back to the project");
    }

    #[test]
    fn a_template_that_exits_mid_request_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = fake_schema(
            tmp.path(),
            r#"echo '{"ready":true}'; read req; idof "$req"; reply '{"pid":0}'; exit 0"#,
        );
        let t = Template::start(&schema).unwrap();
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
  idof "$req"; reply '{{"pid":0}}'
  case "$req" in
    *after*) reply '{resumed}' ;;
    *) reply '{{"status":11,"unfinished":"t.py::net"}}' ;;
  esac
done"#
            );
            let schema = fake_schema(tmp.path(), &script);
            verify(
                &Template::start(&schema).unwrap(),
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
    fn the_survivor_audit_picks_the_first_and_a_stable_five_percent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        let picks = |runner: &SchemaRunner<PytestRunner>| -> Vec<bool> {
            (0..4000)
                .map(|i| runner.audit_pick(&format!("src/m.py@{i}:op:a->b")))
                .collect()
        };
        let (a, b) = (fake_runner(&dir), fake_runner(&dir));
        let (pa, pb) = (picks(&a), picks(&b));
        assert!(pa[0], "the first survivor is always audited");
        assert_eq!(pa[1..], pb[1..], "the same mutants every run");
        let n = pa[1..].iter().filter(|&&p| p).count();
        assert!((140..=260).contains(&n), "~5% of 3999: {n}");
    }

    #[test]
    fn a_tiny_schema_skips_the_template() {
        assert!(!worth_a_template(0, 14));
        assert!(!worth_a_template(27, 14), "under 2 per worker");
        assert!(worth_a_template(28, 14));
        assert!(worth_a_template(2, 1));
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
                 while read req; do echo \"$req\" >> '{}'; idof \"$req\"; reply '{{\"pid\":0}}'; reply '{no_mutant}'; done",
                log.display()
            );
            let schema = fake_schema(tmp.path(), &script);
            let runner = fake_runner(&tmp.path().canonicalize().unwrap());
            let t = Template::start(&schema).unwrap();
            let ran = vec!["t.py::a".to_string(), "t.py::b".to_string()];
            assert_eq!(runner.confirm(&t, &ran), kill_stands);
            assert_eq!(runner.confirm(&t, &ran), kill_stands, "cached");
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

    /// A schemata runner serving one mutant (index 5) from a fake template
    /// that hangs it in `t.py::loops` and answers a no-mutant run with
    /// `no_mutant`. The spawn path can't run anything (`python = /bin/false`).
    fn hanging(
        dir: &std::path::Path,
        no_mutant: &str,
    ) -> (SchemaRunner<PytestRunner>, Arc<Mutant>) {
        let script = format!(
            r#"echo '{{"ready":true}}'
while read req; do
  idof "$req"; reply '{{"pid":0}}'
  case "$req" in
    *'"idx":5'*) sleep 1; reply '{{"status":9,"timed_out":true,"unfinished":"t.py::loops","ran":["t.py::a","t.py::loops"]}}' ;;
    *) reply '{no_mutant}' ;;
  esac
done"#
        );
        let mut schema = fake_schema(dir, &script);
        let mutant = some_mutant();
        schema.index.insert(mutant.id.clone(), 5);
        let mut runner = fake_runner(&dir.canonicalize().unwrap());
        runner.set_timeout(Duration::from_millis(200));
        assert!(runner.schema.set(Some(schema)).is_ok());
        // Not the always-audited first one; this id is outside the 5% sample.
        runner.first_audited.store(true, Ordering::Relaxed);
        assert!(!runner.audit_pick(&mutant.id));
        (runner, mutant)
    }

    #[test]
    fn a_hang_the_no_mutant_run_clears_is_a_timeout_without_a_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, mutant) = hanging(tmp.path(), r#"{"status":0}"#);
        let o = runner.run(&mutant).unwrap();
        assert!(
            matches!(&o, MutantOutcome::TimedOut { hung_in: Some(t), .. } if t == "t.py::loops"),
            "{o:?}"
        );
        assert_eq!(runner.spawned.load(Ordering::Relaxed), 0, "no spawn re-run");
        assert_eq!(runner.hangs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_hang_the_no_mutant_run_reproduces_goes_to_the_spawn_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, mutant) = hanging(tmp.path(), r#"{"status":256,"failed":["t.py::loops"]}"#);
        let _ = runner.run(&mutant);
        assert_eq!(runner.spawned.load(Ordering::Relaxed), 1);
        assert_eq!(runner.hangs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn verify_fails_only_when_the_helper_itself_breaks() {
        let check = |done: &str, delay: &str, timeout: Duration| {
            let tmp = tempfile::tempdir().unwrap();
            let schema = fake_schema(tmp.path(), &answering(done, delay));
            verify(&Template::start(&schema).unwrap(), (0, 1), timeout)
        };
        let secs = Duration::from_secs(10);
        assert!(check(r#"{"status":0}"#, "0", secs));
        // Failing and hung tests are left to each kill's confirmation.
        assert!(check(r#"{"status":256,"failed":["t.py::a"]}"#, "0", secs));
        assert!(check(r#"{"status":9,"timed_out":true}"#, "0", secs));
        assert!(!check(r#"{"status":768,"error":"Traceback"}"#, "0", secs));
        assert!(!check(r#"{"not":"a result"}"#, "0", secs));
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
    fn each_runner_tells_the_schema_how_to_serve_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        let mirror = super::super::build_mirror(&dir.join("tests"), IsolationMode::Copy).unwrap();
        let json = Path::new("schema.json");
        let m = some_mutant();

        let mut runner = fake_runner(&dir);
        let py = &mut runner.inner;
        py.hypothesis_seed = Some(7);
        py.extra_args = vec!["-k".into(), "x".into()];
        let os = |v: &[&std::ffi::OsStr]| v.iter().map(|s| s.to_os_string()).collect::<Vec<_>>();
        assert_eq!(
            py.serve_args(&mirror, json),
            os(&[
                "serve".as_ref(),
                mirror.root.as_os_str(),
                json.as_os_str(),
                "--tb=no".as_ref(),
                "-q".as_ref(),
                "--hypothesis-seed=7".as_ref(),
                "-k".as_ref(),
                "x".as_ref(),
                mirror.tests.as_os_str(),
            ])
        );
        assert_eq!(SchemaInner::timeout(py), Duration::from_secs(30));
        assert_eq!(
            py.selection(&m, Some("t.py::k")),
            Some(vec!["t.py::k".into()])
        );
        assert!(!py.schema_env().rstest);
        py.exe = "rstest";
        assert!(py.schema_env().rstest, "rstest's vendored pytest");
        assert!(!py.record_all());
        py.record_kill_sets = true;
        assert!(py.record_all(), "a kill set needs every failing test");

        let ut = unittest_runner();
        assert_eq!(
            ut.serve_args(&mirror, json),
            os(&[
                "serve-unittest".as_ref(),
                mirror.root.as_os_str(),
                json.as_os_str(),
                mirror.tests.as_os_str(),
                ut.pattern.as_ref(),
            ])
        );
        assert_eq!(SchemaInner::timeout(&ut), Duration::from_secs(1));
        assert_eq!(ut.selection(&m, None), None, "the whole suite");
        assert_eq!(ut.selection(&m, Some("t.T.k")), Some(vec!["t.T.k".into()]));
        assert!(!ut.record_all());
    }

    #[test]
    fn a_served_kill_records_its_kill_set_only_when_asked_and_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = fake_runner(&tmp.path().canonicalize().unwrap());
        let py = &mut runner.inner;
        let m = some_mutant();
        let mut kill = served(Some(1), None);
        kill.result.failures = vec!["t.py::a".into()];
        for (record_kill_sets, record, recorded) in
            [(false, true, 0), (true, false, 0), (true, true, 1)]
        {
            py.record_kill_sets = record_kill_sets;
            let o = py.served_outcome(&m, &kill, record);
            assert_eq!(o.status_label(), "killed");
            assert_eq!(
                py.take_kill_sets().len(),
                recorded,
                "record_kill_sets={record_kill_sets} record={record}"
            );
        }
    }

    /// A schemata runner serving one mutant (index 5) from a fake template
    /// that answers it with `done` and a no-mutant run with a pass.
    fn serving(dir: &std::path::Path, done: &str) -> (SchemaRunner<PytestRunner>, Arc<Mutant>) {
        let script = format!(
            r#"echo '{{"ready":true}}'
while read req; do
  idof "$req"; reply '{{"pid":0}}'
  case "$req" in
    *'"idx":5'*) reply '{done}' ;;
    *) reply '{{"status":0}}' ;;
  esac
done"#
        );
        let mut schema = fake_schema(dir, &script);
        let mutant = some_mutant();
        schema.index.insert(mutant.id.clone(), 5);
        let runner = fake_runner(&dir.canonicalize().unwrap());
        assert!(runner.schema.set(Some(schema)).is_ok());
        (runner, mutant)
    }

    #[test]
    fn an_audit_stands_on_a_usable_fork_and_falls_back_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let (runner, m) = serving(
            tmp.path(),
            r#"{"status":256,"failed":["t.py::a"],"ran":["t.py::a"]}"#,
        );
        let o = runner.audit(&m, "t.py::a").unwrap();
        assert_eq!(o.map(|o| o.status_label()), Some("killed"));

        // A broken child: the spawn path answers, and it can't run here.
        let tmp = tempfile::tempdir().unwrap();
        let (runner, m) = serving(tmp.path(), r#"{"status":768,"error":"Traceback"}"#);
        assert!(
            !matches!(runner.audit(&m, "t.py::a"), Ok(Some(_))),
            "not scored from the fork"
        );
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

    #[test]
    fn recv_names_a_silent_and_an_exited_template() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let err = recv(&rx, Duration::from_millis(10)).unwrap_err();
        assert!(err.to_string().contains("no answer within"), "{err}");
        tx.send("hi".into()).unwrap();
        assert_eq!(recv(&rx, Duration::from_millis(10)).unwrap(), "hi");
        drop(tx);
        let err = recv(&rx, Duration::from_millis(10)).unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
    }
}
