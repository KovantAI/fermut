//! Test runner trait and shared mirror/patch plumbing.
//!
//! A `Runner` takes a `Mutant`, applies the source patch in an isolated
//! mirror of the project, invokes a Python test framework against the mirror,
//! and returns the outcome.
//!
//! Mirrors are built **once per worker thread** and reused across every
//! mutant that thread processes. Per mutant we only splice the patched file
//! and revert it after the test exits, instead of recopying the whole tree.
//! That turns N copies of the project (one per mutant) into N_jobs copies
//! (one per rayon worker).

pub(crate) mod exit;
pub mod kill_sets;
pub(crate) mod memory;
pub(crate) mod pristine;
pub(crate) mod process_group;
pub mod pytest;
pub(crate) mod pytest_result;
pub mod python;
#[cfg(unix)]
pub mod schema;
pub mod unittest;

pub use python::{interpreter, resolve_python, resolve_tool};

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{Config, IsolationMode, RunnerKind};
use crate::emit::patch_source;
use crate::mutator::Mutant;
use crate::report::MutantOutcome;

pub trait Runner: Send + Sync {
    /// Takes `&Arc<Mutant>` so the outcome it returns embeds the mutant by a
    /// refcount bump, not a deep copy. Helpers that only read the mutant still
    /// take `&Mutant` and are called via deref coercion.
    fn run(&self, mutant: &std::sync::Arc<Mutant>) -> Result<MutantOutcome>;

    /// Run the *unmutated* test suite once and report whether it is green.
    ///
    /// A red or erroring suite makes every covered mutant exit non-zero, which
    /// the per-mutant logic counts as "killed" — inflating the mutation score
    /// toward 100% while proving nothing. The engine calls this before any
    /// mutation and aborts on [`BaselineStatus::Failed`].
    fn baseline(&self) -> Result<BaselineStatus>;

    /// Called once with every collected mutant, before any `run`. Default
    /// no-op; the schemata runner keeps them to build its schema from.
    fn prepare(&self, _mutants: &[std::sync::Arc<Mutant>]) -> Result<()> {
        Ok(())
    }

    /// The tests the last green [`baseline`](Self::baseline) deselected
    /// (`--deselect`, `-k`, `-m` among the pytest args), so the engine can
    /// narrow coverage selection to what pytest actually runs. Default `None`:
    /// not known.
    fn baseline_deselection(&self) -> Option<Deselection> {
        None
    }

    /// Replace the per-mutant timeout, for the `--timeout-factor` budget sized
    /// after the baseline. Default no-op, for test doubles that never time out.
    fn set_timeout(&mut self, _timeout: Duration) {}

    /// Cap each mutant's memory at `bytes` (see [`memory`]), sized after the
    /// baseline so the baseline itself runs uncapped. Default no-op.
    fn set_memory_cap(&mut self, _bytes: u64) {}

    /// Drain the kills learned this run (which test killed which
    /// `(file, operator)`), for the engine to fold into the smart-ordering
    /// sidecar. Default empty — only the pytest runner learns them, and only
    /// when smart ordering is on.
    fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
        Vec::new()
    }

    /// Drain the per-mutant kill-sets recorded this run, for the engine to write
    /// out when `--record-kill-sets` is set. Default empty — only the pytest
    /// runner records them, and only when the flag is on.
    fn take_kill_sets(&self) -> Vec<kill_sets::KillSetRecord> {
        Vec::new()
    }

    /// Re-run `mutant` against only the test `killer` that a cached verdict
    /// says killed it — the killer-keyed cache's sampled audit. `Ok(None)`
    /// when the runner can't select a single test (the default), in which case
    /// the cached kill is left as is.
    fn audit(
        &self,
        _mutant: &std::sync::Arc<Mutant>,
        _killer: &str,
    ) -> Result<Option<MutantOutcome>> {
        Ok(None)
    }
}

/// What a baseline run's pytest deselected, and the tests it ran, by
/// coverage node id.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Deselection {
    pub deselected: std::collections::BTreeSet<String>,
    pub ran: std::collections::BTreeSet<String>,
}

/// Result of the pre-flight baseline run (the unmutated suite).
#[derive(Debug)]
pub enum BaselineStatus {
    /// The suite exited 0 — safe to proceed with mutation. `wall` is how long
    /// the test process ran (mirror setup excluded); it sizes `--timeout-factor`.
    Passed { wall: Duration },
    /// The suite failed/errored. `output` is the tail of pytest's combined
    /// stdout/stderr, for surfacing in the abort message.
    Failed { output: String },
}

impl std::fmt::Display for BaselineStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BaselineStatus::Passed { .. } => "passed",
            BaselineStatus::Failed { .. } => "failed",
        })
    }
}

/// Build a one-shot, unprecompiled mirror (tests).
#[cfg(test)]
pub(crate) fn build_mirror(tests: &Path, mode: IsolationMode) -> Result<Mirror> {
    Mirror::build(tests, mode)
}

/// Build a one-shot mirror for a baseline run, precompiled like every worker
/// mirror: the baseline must see the same tree as the mutant runs
/// (`__pycache__` included), or a test that lists the package passes the
/// baseline and fails every mutant. Unlike [`with_worker_mirror`], this isn't
/// cached in a thread-local — the baseline runs once on the calling thread
/// before the rayon pool spins up, so there is nothing to reuse.
pub(crate) fn build_baseline_mirror(
    tests: &Path,
    mode: IsolationMode,
    python: Option<&Path>,
) -> Result<Mirror> {
    let mirror = Mirror::build(tests, mode)?;
    precompile(&mirror.root, &python::pytest_interpreter(python));
    Ok(mirror)
}

/// PYTHONPATH that makes the mirror's (possibly mutated) package win over an
/// editable install whose `.pth` points at the original source tree. Prepends
/// the mirror's `src/` and root, then the ambient PYTHONPATH. Shared by the
/// per-mutant run and the baseline run so both resolve imports identically.
pub(crate) fn mirror_pythonpath(mirror: &Mirror) -> Result<std::ffi::OsString> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mirror_src = mirror.root.join("src");
    if mirror_src.is_dir() {
        paths.push(mirror_src);
    }
    paths.push(mirror.root.clone());
    if let Some(existing) = std::env::var_os("PYTHONPATH") {
        paths.extend(std::env::split_paths(&existing).filter(|p| !p.as_os_str().is_empty()));
    }
    std::env::join_paths(&paths).context("joining PYTHONPATH")
}

/// Caller-shell variables that silently change how the suite runs — and so
/// can change verdicts — without appearing in the project's config.
/// `PYTEST_ADDOPTS` / `PYTEST_PLUGINS` inject flags or plugins (`-n auto`,
/// `--cov`, `-p ...`) into every spawned pytest; `PYTHONWARNINGS` can turn a
/// warning into a failure (a false kill); `PYTHONOPTIMIZE` strips `assert`
/// from the code under test; `PYTHONINSPECT` drops the child into a REPL that
/// blocks until the timeout. Project-level settings belong in `pytest_args`
/// or the project's pytest config, which still apply.
pub(crate) const SANITIZED_ENV_VARS: &[&str] = &[
    "PYTEST_ADDOPTS",
    "PYTEST_PLUGINS",
    "PYTHONWARNINGS",
    "PYTHONOPTIMIZE",
    "PYTHONINSPECT",
];

/// The `PYTHONHASHSEED` every Python process fermut starts runs under.
/// Python fixes it at interpreter start and forks inherit it, so a random seed
/// gives order-dependent mutants (iterating a set of str) a verdict that
/// changes from run to run, and under schemata all of them flip together.
/// Pinned, every run, the baseline, and served and spawned mutants alike see
/// one iteration order; finding order-dependent tests is the test tool's job.
/// A seed the user exports wins (`PYTHONHASHSEED=random` opts out).
/// Non-zero: `0` would switch hash randomization off, which a few suites check.
pub(crate) const DEFAULT_HASH_SEED: &str = "1";

/// The seed [`configure_mirror_cmd`] sets: the user's `PYTHONHASHSEED`, else
/// [`DEFAULT_HASH_SEED`]. Part of the cache key and the history config hash.
pub(crate) fn hash_seed() -> String {
    hash_seed_from(std::env::var_os("PYTHONHASHSEED").as_deref())
}

fn hash_seed_from(user: Option<&std::ffi::OsStr>) -> String {
    user.filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_HASH_SEED.to_string())
}

/// Set on every suite run fermut spawns. The `pytest --fermut` front-end
/// (`python/pytest_fermut/plugin.py`) stays inert when it sees this, so a
/// project with `--fermut` in its pytest `addopts` can't recurse into another
/// mutation run from inside fermut's own baseline, coverage, or mutant runs.
pub(crate) const CHILD_ENV: &str = "FERMUT_CHILD";

/// Strip [`SANITIZED_ENV_VARS`] from `cmd` so every suite run is reproducible
/// from the project alone, independent of the caller's shell, and mark it as
/// a fermut child ([`CHILD_ENV`]).
pub(crate) fn sanitize_python_env(cmd: &mut std::process::Command) {
    for var in SANITIZED_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env(CHILD_ENV, "1");
}

/// Apply the environment every per-mutant framework command against a mirror
/// needs: run from the mirror root, in a fresh process group (so a timeout kill
/// reaches child processes), with `PYTHONPATH` pinned to the mirror (beats an
/// editable install's `.pth` pointing at the original tree) and bytecode writes
/// disabled (a stale `.pyc` that still validates against the patched source
/// would mask the mutation and falsely report a survivor). Shared by the pytest
/// and unittest runners so these correctness-critical knobs can't drift apart.
pub(crate) fn configure_mirror_cmd(
    cmd: &mut std::process::Command,
    mirror: &Mirror,
    private_home: bool,
) -> Result<()> {
    cmd.current_dir(&mirror.root);
    process_group::with_new_process_group(cmd);
    cmd.env("PYTHONPATH", mirror_pythonpath(mirror)?);
    cmd.env("PYTHONDONTWRITEBYTECODE", "1");
    cmd.env("PYTHONHASHSEED", hash_seed());
    sanitize_python_env(cmd);
    // A pytest temp root per mirror. Parallel sessions sharing
    // `$TMPDIR/pytest-of-$USER` race in its numbered-dir cleanup; the
    // "(rm_rf) error removing" / `FileNotFoundError` warnings that leaves fail
    // the run under `filterwarnings = error` (pallets, pyjwt): ~2% of runs of
    // a passing suite at 14 workers, each a false kill. A mirror runs one
    // pytest at a time, so its own root can't race.
    cmd.env("PYTEST_DEBUG_TEMPROOT", &mirror.pytest_temproot);
    // Both roots, so the reporter plugin can give a parametrize value holding
    // the copy's path the id pytest gives it in the project (pytest 9.2 ids a
    // value over 100 characters by name, so the path's length can change it).
    cmd.env(MIRROR_ROOT_ENV, shown_path(&mirror.root));
    cmd.env(PROJECT_ROOT_ENV, &mirror.project_shown);
    // A private home per mirror, same reason: tests that create and delete
    // an app dir under `~` (typer's `get_app_dir`) fail each other's runs
    // across workers, each failure a false kill, and write into the real
    // home. The schemata template gives each fork its own (`PRIVATE_HOME_ENV`).
    if private_home {
        set_home(cmd, &mirror.home, cfg!(windows));
        cmd.env(PRIVATE_HOME_ENV, "1");
    }
    if let Some(suffix) = db_suffix() {
        cmd.env(DB_SUFFIX_ENV, suffix);
    }
    Ok(())
}

/// pytest-django appends this variable's value to every test database name
/// (`test_hc` → `test_hc_fm2`; in-memory SQLite is left alone). Without it,
/// concurrent workers all create the same Postgres/MySQL test DB: the losers
/// fail `database "test_hc" already exists`, every test errors, and each of
/// those mutants is a false kill (healthchecks `--jobs 4`: 100 % vs 81.9 %).
pub(crate) const DB_SUFFIX_ENV: &str = "TOX_PARALLEL_ENV";

/// This worker's test-DB suffix: `fm<rayon worker index>`, after the user's
/// own `TOX_PARALLEL_ENV` (inside `tox -p`) when set. Per worker, not per
/// mutant: a worker runs one suite at a time on either path (the schemata
/// template is sent it per request), and a DB a killed run leaves behind is
/// clobbered by that worker's next run instead of piling up. None outside the
/// worker pool (baseline): the user's value, unchanged.
pub(crate) fn db_suffix() -> Option<String> {
    db_suffix_from(
        std::env::var(DB_SUFFIX_ENV).ok().as_deref(),
        rayon::current_thread_index(),
    )
}

fn db_suffix_from(user: Option<&str>, worker: Option<usize>) -> Option<String> {
    let user = user.filter(|u| !u.is_empty());
    match (user, worker) {
        (Some(u), Some(k)) => Some(format!("{u}_fm{k}")),
        (None, Some(k)) => Some(format!("fm{k}")),
        (u, None) => u.map(str::to_string),
    }
}

/// Tells the schemata template to give each fork its own home.
pub(crate) const PRIVATE_HOME_ENV: &str = "FERMUT_PRIVATE_HOME";

/// The mirror's root and the real project's, as Python prints them; read by
/// `_fermut_reporter` (see its `_stable_long_str_ids`).
pub(crate) const MIRROR_ROOT_ENV: &str = "FERMUT_MIRROR_ROOT";
pub(crate) const PROJECT_ROOT_ENV: &str = "FERMUT_PROJECT_ROOT";

/// `HOME` and the XDG base dirs, all inside `home`. Setting the XDG dirs
/// (not just removing them) matters: an exported `XDG_CONFIG_HOME` would
/// otherwise stay shared. On Windows Python resolves `~` from `USERPROFILE`
/// (it ignores `HOME`), and click/typer's `get_app_dir` reads `APPDATA` /
/// `LOCALAPPDATA`, so those move too. `windows` is a parameter, not a
/// `cfg!`, so the Windows branch is testable on any host.
fn set_home(cmd: &mut std::process::Command, home: &Path, windows: bool) {
    cmd.env("HOME", home);
    for (var, rel) in XDG_DIRS {
        cmd.env(var, home.join(rel));
    }
    if windows {
        cmd.env("USERPROFILE", home);
        for (var, rel) in WINDOWS_DIRS {
            cmd.env(var, home.join(rel));
        }
    }
}

/// Windows' per-user app dirs and their defaults under `USERPROFILE`.
const WINDOWS_DIRS: [(&str, &str); 2] = [
    ("APPDATA", r"AppData\Roaming"),
    ("LOCALAPPDATA", r"AppData\Local"),
];

/// XDG base dirs and their spec defaults under `HOME`; kept in step with the
/// schemata helper's per-fork copy (`Template.child`).
const XDG_DIRS: [(&str, &str); 4] = [
    ("XDG_CONFIG_HOME", ".config"),
    ("XDG_CACHE_HOME", ".cache"),
    ("XDG_DATA_HOME", ".local/share"),
    ("XDG_STATE_HOME", ".local/state"),
];

/// Run `f` against this worker's mirror with `mutant`'s patch spliced in for the
/// duration of the call: reuse (or lazily build) the thread-local mirror, apply
/// the patch, invoke `f`, then revert the patch on scope exit via the guard.
/// Both runners funnel through here so the mirror-reuse + patch-lifecycle wiring
/// lives in exactly one place.
pub(crate) fn run_patched<F>(
    tests: &Path,
    mode: IsolationMode,
    python: Option<&Path>,
    mutant: &Mutant,
    f: F,
) -> Result<MutantOutcome>
where
    F: FnOnce(&Mirror) -> Result<MutantOutcome>,
{
    with_worker_mirror(tests, mode, python, |mirror| {
        let _guard = apply_patch(mirror, mutant)?;
        f(mirror)
    })
}

/// Keep the last `max_lines` lines of `s`, prefixing an elision marker when
/// truncated. Used to bound baseline-failure output in the abort message.
pub(crate) fn tail_lines(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.trim_end().to_string();
    }
    let kept = &lines[lines.len() - max_lines..];
    format!(
        "… ({} earlier lines elided)\n{}",
        lines.len() - max_lines,
        kept.join("\n")
    )
}

/// Spawn `cmd` for a baseline run, capping wall-clock at `timeout`.
///
/// Child stdout/stderr are redirected to temp files (not OS pipes) so a large
/// suite can't deadlock on a full pipe buffer, and so the tail is still
/// readable after a timeout kill. `cmd` must already be configured with its
/// process group via [`with_new_process_group`] so the kill reaches children.
pub(crate) fn run_baseline_with_timeout(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> Result<BaselineStatus> {
    use std::time::Instant;
    use wait_timeout::ChildExt;

    let out_file = tempfile::NamedTempFile::new().context("creating baseline stdout buffer")?;
    let err_file = tempfile::NamedTempFile::new().context("creating baseline stderr buffer")?;
    cmd.stdout(
        out_file
            .reopen()
            .context("reopening baseline stdout buffer")?,
    );
    cmd.stderr(
        err_file
            .reopen()
            .context("reopening baseline stderr buffer")?,
    );
    // Same staleness guard as the per-mutant run: don't write `.pyc` into the
    // mirror during the baseline pass.
    cmd.env("PYTHONDONTWRITEBYTECODE", "1");
    sanitize_python_env(&mut cmd);

    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The test runner binary isn't on PATH. A bare `No such file or
            // directory (os error 2)` here is the classic first-run faceplant
            // (forgot to activate the venv, pytest not installed). Name the
            // program and point at the preflight.
            let prog = cmd.get_program().to_string_lossy().into_owned();
            anyhow::bail!(
                "could not start the test runner: `{prog}` was not found on PATH.\n\n\
                 Install it and activate your virtualenv if you use one, or pass \
                 `--python /path/to/venv`, then verify with `fermut doctor`:\n  \
                 pytest  → `pip install pytest pytest-cov`\n  \
                 unittest → ensure `python` is on PATH\n\n\
                 More: {}",
                crate::TROUBLESHOOTING_URL
            );
        }
        Err(e) => return Err(e).context("spawning baseline test process"),
    };
    // No cap on the baseline, but Windows measures its peak this way (the
    // auto `--max-memory` cap is sized from it); elsewhere a no-op.
    let _measure = memory::watch(child.id(), None);

    let read_tail = || -> String {
        let mut s = std::fs::read_to_string(out_file.path()).unwrap_or_default();
        s.push_str(&std::fs::read_to_string(err_file.path()).unwrap_or_default());
        tail_lines(&s, 40)
    };

    match child
        .wait_timeout(timeout)
        .context("waiting on baseline run")?
    {
        Some(status) if status.success() => Ok(BaselineStatus::Passed {
            wall: started.elapsed(),
        }),
        Some(_) => Ok(BaselineStatus::Failed {
            output: read_tail(),
        }),
        None => {
            process_group::kill_group(&mut child);
            let _ = child.wait();
            Ok(BaselineStatus::Failed {
                output: format!(
                    "baseline suite exceeded --baseline-timeout ({}s) and was killed after \
                     {}s — raise --baseline-timeout if your suite legitimately runs longer, \
                     or fix a hanging test.\n{}",
                    timeout.as_secs(),
                    started.elapsed().as_secs(),
                    read_tail()
                ),
            })
        }
    }
}

/// Pick the configured runner.
pub fn build(cfg: &Config) -> Box<dyn Runner> {
    let tests = cfg.tests_path();
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let baseline_timeout = Duration::from_secs(cfg.baseline_timeout_secs);
    let isolation = cfg.isolation;
    // Smart ordering: load the kill-order history once (immutable, shared for
    // no-lock ordering) and a fresh sink the runner appends kills to. When off,
    // the history is empty (ordering no-ops) and no kills are recorded.
    let kill_order = std::sync::Arc::new(if cfg.smart_order {
        crate::kill_order::KillOrder::load(&cfg.kill_order_path)
    } else {
        crate::kill_order::KillOrder::default()
    });
    let kill_sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    // Kill-set recording (the higher-order-mutant experiment): a shared sink the
    // runner appends per-mutant kill-sets to, drained by the engine afterward.
    // Empty and unused unless `--record-kill-sets` is set.
    let record_kill_sets = cfg.record_kill_sets.is_some();
    let kill_set_sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let key_base = cfg.source_root.clone();
    // `--select-tests`: verify only the tests mutants will run against — a
    // broken test outside the selection must not abort a narrowed run.
    let baseline_ids = cfg
        .coverage_unselected
        .as_ref()
        .and(cfg.coverage.as_ref())
        .map(|narrowed| {
            let mut ids: Vec<String> = narrowed.test_ids().cloned().collect();
            ids.sort();
            ids
        });
    // `rstest` is a pytest-CLI-compatible drop-in, so it reuses the pytest
    // runner wholesale — only the framework executable name differs.
    let pytest_config = |exe: &'static str| pytest::PytestConfig {
        tests: tests.clone(),
        timeout,
        baseline_timeout,
        hypothesis_seed: cfg.hypothesis_seed,
        extra_args: cfg.pytest_args.clone(),
        private_home: cfg.private_home,
        isolation,
        coverage: cfg.coverage.clone(),
        python: if exe == "rstest" {
            python::rstest_interpreter(cfg.python.as_deref())
        } else {
            cfg.python.clone()
        },
        exe,
        smart_order: cfg.smart_order,
        kill_order: kill_order.clone(),
        kill_sink: kill_sink.clone(),
        record_kill_sets,
        kill_set_sink: kill_set_sink.clone(),
        // Only coverage-selected runs can name a killer the cache can use.
        learn_killer: cfg.cache && cfg.coverage.is_some(),
        key_base: key_base.clone(),
        plugin: cfg.pytest_plugin,
        baseline_ids: baseline_ids.clone(),
    };
    let pytest_compatible = |exe: &'static str| -> Box<dyn Runner> {
        Box::new(pytest::PytestRunner::new(pytest_config(exe)))
    };
    match cfg.runner {
        #[cfg(unix)]
        RunnerKind::Pytest if cfg.schemata => {
            let inner = pytest::PytestRunner::new(pytest_config("pytest"));
            Box::new(schema::SchemaRunner::new(inner))
        }
        #[cfg(unix)]
        RunnerKind::Rstest if cfg.schemata => {
            let inner = pytest::PytestRunner::new(pytest_config("rstest"));
            Box::new(schema::SchemaRunner::new(inner))
        }
        RunnerKind::Pytest => pytest_compatible("pytest"),
        RunnerKind::Rstest => pytest_compatible("rstest"),
        RunnerKind::Unittest => {
            let inner = unittest::UnittestRunner::new(
                tests,
                timeout,
                baseline_timeout,
                isolation,
                cfg.private_home,
                // Resolve the interpreter once: the configured `--python` when
                // set, else a PATH-probed `python3`/`python`. `unittest` has no
                // console script, so it's always `<interp> -m unittest`.
                interpreter(cfg.python.as_deref()),
                cfg.unittest_pattern.clone(),
            );
            #[cfg(unix)]
            if cfg.schemata {
                return Box::new(schema::SchemaRunner::new(inner));
            }
            Box::new(inner)
        }
    }
}

/// Per-worker mirror of the project tree. Built once on first use, then
/// reused for every mutant that worker handles.
pub(crate) struct Mirror {
    _workdir: tempfile::TempDir,
    pub root: PathBuf,
    pub tests: PathBuf,
    pub project_root: PathBuf,
    /// `project_root` resolved, as it appears inside a node id (pytest starts
    /// from `getcwd()` and escapes the id; see [`id_path`]). Ids recorded in
    /// the real project embed it.
    project_real: String,
    /// `project_root` resolved, as Python prints it (unescaped).
    project_shown: String,
    /// `root` as it appears inside a node id, for mapping ids.
    root_shown: String,
    /// pytest's temp root for runs in this mirror (`PYTEST_DEBUG_TEMPROOT`),
    /// beside the project copy. See [`configure_mirror_cmd`].
    pub pytest_temproot: PathBuf,
    /// The private `HOME` for runs in this mirror, beside the project copy.
    pub home: PathBuf,
    /// Directory beside (not inside) the project copy holding fermut's pytest
    /// reporter plugin and its per-run result files. Kept out of `root` so it
    /// can't be collected, imported by the project, or swept into a test path.
    pub plugin_dir: PathBuf,
}

/// Build a mirror at `tests`'s project root, drop it, return wall-clock
/// elapsed. Exposed so external benches (`examples/bench_isolation.rs`)
/// can measure each `IsolationMode` without poking at internal types.
#[doc(hidden)]
pub fn time_mirror_build(tests: &Path, mode: IsolationMode) -> Result<Duration> {
    let start = std::time::Instant::now();
    let m = Mirror::build(tests, mode)?;
    let elapsed = start.elapsed();
    drop(m);
    Ok(elapsed)
}

impl Mirror {
    /// The mirror copy of project file `file`.
    ///
    /// `file` and `project_root` can spell the same directory differently: on
    /// macOS the temp dir is `/var/...` but its canonical form is
    /// `/private/var/...`, and the CLI path and config-resolved `tests` path
    /// don't agree on which one they use. A plain `strip_prefix` then fails,
    /// so retry with both sides canonicalized. Never fall back to `file`
    /// itself: `root.join(<absolute>)` is that absolute path, so the mutant
    /// would be written into the user's real source tree.
    fn target_for(&self, file: &Path) -> Result<PathBuf> {
        if let Ok(rel) = file.strip_prefix(&self.project_root) {
            return Ok(self.root.join(rel));
        }
        let canon_file = file
            .canonicalize()
            .with_context(|| format!("resolving mutant file {}", file.display()))?;
        let canon_root = self
            .project_root
            .canonicalize()
            .with_context(|| format!("resolving project root {}", self.project_root.display()))?;
        let rel = canon_file.strip_prefix(&canon_root).with_context(|| {
            format!(
                "mutant file {} is outside the mirrored project root {}",
                file.display(),
                self.project_root.display()
            )
        })?;
        Ok(self.root.join(rel))
    }

    fn build(tests: &Path, mode: IsolationMode) -> Result<Self> {
        let workdir = tempfile::tempdir().context("creating workdir")?;
        let project_root = find_project_root(tests).unwrap_or_else(|| {
            tests
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| tests.to_path_buf())
        });
        // Resolved, so paths handed to pytest match its `getcwd()`. On macOS the
        // temp dir sits behind the `/var` → `/private/var` symlink, and pytest
        // would otherwise print ids for an absolute test path as
        // `../../var/…/project/tests/…`, unique per mirror.
        let base = workdir.path().canonicalize().context("resolving workdir")?;
        let root = base.join("project");
        copy_dir_all(&project_root, &root, mode)?;
        let tests_rel = tests.strip_prefix(&project_root).unwrap_or(tests);
        let tests_in_mirror = root.join(tests_rel);
        // Short name: tmp_path lands under it, and AF_UNIX socket paths in
        // tests are limited to ~104 bytes.
        let pytest_temproot = base.join("t");
        std::fs::create_dir_all(&pytest_temproot)
            .with_context(|| format!("mkdir -p {}", pytest_temproot.display()))?;
        let home = base.join("h");
        std::fs::create_dir_all(&home).with_context(|| format!("mkdir -p {}", home.display()))?;
        let plugin_dir = base.join("fermut-plugin");
        std::fs::create_dir_all(&plugin_dir)
            .with_context(|| format!("mkdir -p {}", plugin_dir.display()))?;
        let module = plugin_dir.join(format!("{}.py", pytest_result::REPORTER_MODULE));
        std::fs::write(&module, pytest_result::REPORTER_SOURCE)
            .with_context(|| format!("writing {}", module.display()))?;
        let project_shown = shown_path(
            &project_root
                .canonicalize()
                .unwrap_or_else(|_| project_root.clone()),
        );
        let project_real = py_unicode_escape(&project_shown);
        let root_shown = id_path(&root);
        Ok(Self {
            _workdir: workdir,
            root,
            tests: tests_in_mirror,
            project_root,
            project_real,
            project_shown,
            root_shown,
            pytest_temproot,
            home,
            plugin_dir,
        })
    }

    /// A node id recorded in the real project, as pytest names it in this
    /// mirror. Parametrize ids can embed an absolute path (werkzeug:
    /// `test_from_directory[/repo/tests/res-test.txt]`); in the mirror pytest
    /// generates the mirror's path instead, so the recorded id would not
    /// resolve (pytest exit 4, scored `error`). Only the part after the first
    /// `::` is rewritten; the file part is relative.
    pub(crate) fn id_into_mirror(&self, id: &str) -> String {
        match id.split_once("::") {
            Some((file, rest)) => format!(
                "{file}::{}",
                replace_path(rest, &self.project_real, &self.root_shown)
            ),
            None => id.to_string(),
        }
    }

    /// pytest output or reporter results from a run in this mirror, with the
    /// mirror path mapped back to the real project's, so node ids it names
    /// (killer, kill-set, `hung_in`) match the recorded coverage ids.
    pub(crate) fn text_from_mirror(&self, text: &str) -> String {
        replace_path(text, &self.root_shown, &self.project_real)
    }
}

/// A resolved path as it appears inside a pytest node id: in Python's form
/// ([`shown_path`]), escaped the way pytest escapes string parametrize
/// values (`unicode_escape`: `C:\\x`, `Jos\xe9`).
fn id_path(p: &Path) -> String {
    py_unicode_escape(&shown_path(p))
}

/// Python's `s.encode("unicode_escape").decode("ascii")`.
fn py_unicode_escape(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            ' '..='~' => out.push(c),
            c if (c as u32) < 0x100 => write!(out, "\\x{:02x}", c as u32).unwrap(),
            c if (c as u32) < 0x10000 => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => write!(out, "\\U{:08x}", c as u32).unwrap(),
        }
    }
    out
}

/// A resolved path as Python prints it. Windows `canonicalize` returns the
/// verbatim form (`\\?\C:\x`, `\\?\UNC\host\share`); `os.getcwd()` gives
/// `C:\x` / `\\host\share`, so ids would never match the verbatim string.
fn shown_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
    }
}

/// `text` with each occurrence of the path `from` replaced by `to`, where the
/// match is a whole path: `/repo` maps `/repo/x` and `[/repo]`, not the
/// sibling `/repo2/x` or the relative `fixtures/repo/x`. (A parametrize value
/// that is itself `/repo/...` is still mapped; text can't tell them apart.)
fn replace_path(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (i, _) in text.match_indices(from) {
        let end = i + from.len();
        // `-` joins parametrize values (`[1-/repo/x]`), so it may precede.
        let starts = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || "_./\\".contains(c)));
        let ends = text[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || "_-.".contains(c)));
        if starts && ends {
            out.push_str(&text[last..i]);
            out.push_str(to);
            last = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// RAII guard: restores the original mirror file bytes on drop.
///
/// Holds the pre-patch bytes so a panic, timeout, or early return still
/// returns the worker's mirror to a clean state before the next mutant runs.
pub(crate) struct PatchGuard {
    path: PathBuf,
    original: Option<Vec<u8>>,
}

impl Drop for PatchGuard {
    fn drop(&mut self) {
        let Some(bytes) = self.original.take() else {
            return;
        };
        if std::fs::write(&self.path, &bytes).is_ok() {
            return;
        }
        // Retry once — a transient failure (a concurrent reader still holding
        // the file, a momentary FS hiccup) may clear on a second try.
        if let Err(e) = std::fs::write(&self.path, &bytes) {
            // Drop can't return a Result, but a swallowed restore failure is
            // dangerous: the mirror file is left MUTATED, so every later mutant
            // this worker runs on that file measures against corrupt source and
            // may report bogus survivors/kills. At minimum, name the path.
            tracing::error!(
                path = %self.path.display(),
                error = %e,
                "failed to restore mirror file after mutant (retried once); \
                 file left MUTATED — later mutants on this file in this worker \
                 may report incorrect results"
            );
        }
    }
}

/// Write the mutant's patched source into the mirror, returning a guard
/// that reverts the file when dropped.
pub(crate) fn apply_patch(mirror: &Mirror, mutant: &Mutant) -> Result<PatchGuard> {
    let target = mirror.target_for(&mutant.file)?;

    let original_bytes = std::fs::read(&target)
        .with_context(|| format!("reading mirror file {}", target.display()))?;
    let original_str = std::str::from_utf8(&original_bytes)
        .with_context(|| format!("mirror file not utf8: {}", target.display()))?;
    let patched = patch_source(original_str, mutant.range, &mutant.replacement);

    let guard = PatchGuard {
        path: target.clone(),
        original: Some(original_bytes),
    };
    // Unlink first so hardlinked mirrors don't clobber the source file.
    // Plain `fs::write` reuses the existing inode in O_TRUNC mode; if that
    // inode is shared with the real source tree, the original loses its
    // bytes. Removing + rewriting gives us a fresh inode. Harmless under
    // copy/reflink too.
    let _ = std::fs::remove_file(&target);
    std::fs::write(&target, patched)
        .with_context(|| format!("writing patched mirror at {}", target.display()))?;
    Ok(guard)
}

thread_local! {
    static WORKER_MIRROR: RefCell<Option<(PathBuf, IsolationMode, Mirror, pristine::Pristine)>> =
        const { RefCell::new(None) };
}

/// Run `f` against this worker's mirror, building one on first call.
/// The mirror is rebuilt only if `tests` or isolation mode changes — neither
/// should within a run, or if putting it back after a run fails. A fresh
/// mirror is precompiled for `python` (the runner's `--python`, else pytest's
/// interpreter); see [`precompile`]. After `f`, files its tests left changed
/// are restored ([`pristine`]), so the next mutant sees the tree as built.
pub(crate) fn with_worker_mirror<F, T>(
    tests: &Path,
    mode: IsolationMode,
    python: Option<&Path>,
    f: F,
) -> Result<T>
where
    F: FnOnce(&Mirror) -> Result<T>,
{
    WORKER_MIRROR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let needs_build = !matches!(&*slot, Some((p, m, ..)) if p == tests && *m == mode);
        if needs_build {
            *slot = None;
            let mirror = build_baseline_mirror(tests, mode, python)?;
            let pristine = pristine::Pristine::take(&mirror.root, &mirror.project_root);
            *slot = Some((tests.to_path_buf(), mode, mirror, pristine));
        }
        let (_, _, mirror, pristine) = slot.as_mut().unwrap();
        let out = f(mirror);
        match pristine.restore(&mirror.project_root, mode) {
            Ok(dirty) if !dirty.is_empty() => warn_dirtied(&dirty),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("restoring the mirror after a run failed, rebuilding it: {e:#}");
                *slot = None;
            }
        }
        out
    })
}

fn warn_dirtied(dirty: &[PathBuf]) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    tracing::debug!("restored mirror paths a run left changed: {dirty:?}");
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let shown: Vec<_> = dirty
            .iter()
            .take(5)
            .map(|p| p.display().to_string())
            .collect();
        tracing::warn!(
            "tests changed files inside the project tree and didn't put them back ({}); \
             fermut restores its copy after each mutant, but a test that leaves the tree \
             changed when it fails isn't hermetic",
            shown.join(", ")
        );
    }
}

/// Compile the mirror's sources to checked-hash `.pyc` (PEP 552), so each
/// per-mutant process loads untouched project modules instead of recompiling
/// them (~3–12% per spawn on large projects). Safe against the stale-`.pyc`
/// false survivor that `copy_dir_all` prunes for: a checked-hash `.pyc` is
/// validated by a hash of the source bytes, not (mtime, size), so the mutated
/// file always misses and compiles from source, and `PYTHONDONTWRITEBYTECODE`
/// stops its bytecode being written back. Never use `unchecked-hash` here.
/// Best effort: a failure (unparseable file, wrong interpreter) only costs
/// the speedup.
fn precompile(root: &Path, python: &Path) {
    let out = std::process::Command::new(python)
        .args([
            "-m",
            "compileall",
            "-q",
            "-j",
            "0",
            "--invalidation-mode",
            "checked-hash",
        ])
        .arg(root)
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => tracing::debug!(
            "precompiling mirror: compileall exited {}: {}",
            o.status,
            tail_lines(&String::from_utf8_lossy(&o.stdout), 5)
        ),
        Err(e) => tracing::debug!("precompiling mirror: can't run {}: {e}", python.display()),
    }
}

/// Files that mark a Python project root: pytest's own rootdir set plus
/// `setup.py`. Apps without a `pyproject.toml` (Django, older packages) still
/// carry one of these, so the walk doesn't climb into an unrelated ancestor.
const ROOT_MARKERS: &[&str] = &[
    "pyproject.toml",
    "setup.cfg",
    "pytest.ini",
    ".pytest.ini",
    "tox.ini",
    "setup.py",
];

/// Whether `p` is a project root: it holds a [`ROOT_MARKERS`] file or `.git`
/// (a file in worktrees/submodules, hence `exists`). `.git` caps the walk at
/// the repo boundary when no marker sits inside it.
pub(crate) fn is_project_root(p: &Path) -> bool {
    ROOT_MARKERS.iter().any(|m| p.join(m).exists()) || p.join(".git").exists()
}

pub(crate) fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|p| is_project_root(p))
        .map(Path::to_path_buf)
}

fn copy_dir_all(src: &Path, dst: &Path, mode: IsolationMode) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("mkdir -p {}", dst.display()))?;
    // Mirror only the working source tree. A naive recursive copy duplicates the
    // entire project root per mutant — virtualenvs (`.venv`/`venv`), VCS metadata
    // (`.git`), tool caches, `node_modules`, local data dirs, and editor/agent
    // state (e.g. `.claude`). On a real project that can be gigabytes copied to
    // run tests against ~megabytes of source.
    //
    // What we prune:
    //   - named junk directories (`.git`, `.venv`, `.claude`, `.pytest_cache`, ...;
    //     see `util::is_pruned_dir_name`). Other hidden dirs like `.github` are
    //     kept: tests may read scripts or fixtures from them.
    //   - `__pycache__` directories and stray `.pyc`/`.pyo` files. CRITICAL for
    //     correctness, not just size: the mirror is reused across mutants, and a
    //     stale compiled `.pyc` whose (mtime, size) still validates against the
    //     patched `.py` makes Python import the ORIGINAL bytecode — the mutation
    //     silently doesn't apply and the mutant is falsely reported survived.
    //     Same-size mutations (e.g. `<`→`>`) are the worst case since the size
    //     check can't catch them. We also set PYTHONDONTWRITEBYTECODE on the test
    //     subprocess so pytest can't write fresh `.pyc` back mid-run.
    //   - directories matched by `.gitignore` (`venv`, `node_modules`, data dirs,
    //     ...), and ignored files outside Python packages (`coverage.json`, logs).
    //     Ignored files inside a package are build output the package imports
    //     or reads (`*.so`, compiled `*.mo`), so `copy_ignored_files` keeps them.
    //
    // What we keep: hidden *files*. Dotfiles like `.env`, `.python-version`,
    // `.coveragerc`, `.flake8` are tiny but can change test behavior, so `hidden`
    // filtering stays off and we only drop the named directories. `require_git(false)`
    // keeps `.gitignore` honored even though the walk root need not be a git
    // checkout. Third-party deps load from the ambient interpreter (the `pytest`
    // on PATH), never from a copied virtualenv, so dropping `.venv` is safe.
    let walker = ignore::WalkBuilder::new(src)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .filter_entry(|entry| {
            // Never prune the walk root.
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let name = entry.file_name().to_str();
            // Drop junk directories and `__pycache__` (stale bytecode).
            !(is_dir && name.is_some_and(crate::util::is_pruned_dir_name))
        })
        .build();
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(src).unwrap();
        let target = dst.join(rel);
        let file_type = entry
            .file_type()
            .with_context(|| format!("no file type for {}", path.display()))?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("create mirror dir {}", target.display()))?;
            let in_package = rel
                .ancestors()
                .take_while(|a| !a.as_os_str().is_empty())
                .any(|a| src.join(a).join("__init__.py").is_file());
            copy_ignored_files(path, &target, mode, in_package)?;
        } else if file_type.is_file() {
            // Skip stray compiled bytecode outside `__pycache__` (e.g. legacy
            // sidecar `.pyc`); same staleness hazard as above.
            if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("pyc") | Some("pyo")
            ) {
                continue;
            }
            // Already copied by its directory's `copy_ignored_files` pass.
            if target.is_file() {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create mirror dir {}", parent.display()))?;
            }
            // A file deleted mid-walk (a filter's `<stem>_fermut_*.py` temp file,
            // written beside the source while workers build mirrors) is no
            // longer part of the project.
            if let Err(e) = clone_file(path, &target, mode) {
                if path.exists() {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Copy the files directly in `dir` that `.gitignore` excluded but that are
/// part of the importable package:
/// - compiled extension modules (`*.so`, `*.pyd`) anywhere. A C extension
///   built in place (`build_ext --inplace`, editable installs) is what the
///   project imports; without it node ids parametrized on the extension
///   (markupsafe's `[markupsafe._speedups]`) don't exist, so pytest exits 4.
/// - any other file when `dir` sits inside a Python package (`in_package`):
///   build output generated into the package, like flaskbb's compiled `*.mo`
///   catalogues, without which its translation test fails in the mirror only.
///   Stray `.pyc`/`.pyo` stay out (stale-bytecode hazard).
///
/// Only directories the walk mirrors are scanned, so ignored trees (venvs,
/// build dirs) stay out, and out-of-package ignored files (`coverage.json`,
/// logs at the root) aren't copied.
fn copy_ignored_files(
    dir: &Path,
    target: &Path,
    mode: IsolationMode,
    in_package: bool,
) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        let ext = path.extension().and_then(|e| e.to_str());
        let keep = match ext {
            Some("so") | Some("pyd") => true,
            Some("pyc") | Some("pyo") => false,
            _ => in_package,
        };
        let to = target.join(path.file_name().unwrap());
        if keep && path.is_file() && !to.exists() {
            clone_file(&path, &to, mode)?;
        }
    }
    Ok(())
}

/// Materialize one mirror file from `src` to `dst` using the requested
/// isolation strategy, falling back to plain copy when the filesystem says
/// no (`EXDEV`, `EOPNOTSUPP`, ...). Anything other than a copy keeps
/// `apply_patch` from being a no-op safety risk because the patched file
/// is always unlinked before being rewritten.
fn clone_file(src: &Path, dst: &Path, mode: IsolationMode) -> Result<()> {
    let attempt = match mode {
        // `reflink_or_copy` handles macOS clonefile + Linux ficlone + Windows
        // CoW, and falls back to `fs::copy` on filesystems that lack support.
        IsolationMode::Reflink | IsolationMode::Auto => reflink_copy::reflink_or_copy(src, dst)
            .map(|_| ())
            .map_err(anyhow::Error::from),
        IsolationMode::Hardlink => std::fs::hard_link(src, dst)
            .or_else(|_| std::fs::copy(src, dst).map(|_| ()))
            .map_err(anyhow::Error::from),
        IsolationMode::Copy => std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(anyhow::Error::from),
    };
    attempt.with_context(|| {
        format!(
            "materializing {} -> {} (mode={})",
            src.display(),
            dst.display(),
            mode
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        apply_patch, copy_dir_all, py_unicode_escape, replace_path, sanitize_python_env,
        shown_path, tail_lines, Mirror, PatchGuard, CHILD_ENV, SANITIZED_ENV_VARS,
    };

    #[test]
    fn a_mirror_that_cant_be_restored_is_rebuilt() {
        use crate::config::IsolationMode;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        std::fs::write(dir.join("mod.py"), "x = 1").unwrap();
        let tests = dir.join("tests");
        // Precompiling is best effort; `true` skips it.
        let py = std::path::Path::new("true");
        // A fresh thread: the mirror is thread-local.
        std::thread::spawn(move || {
            let run = |edit: bool| {
                super::with_worker_mirror(&tests, IsolationMode::Copy, Some(py), |m| {
                    if edit {
                        // Changed in the mirror, gone from the source: restore
                        // has nothing to copy back and fails.
                        std::fs::write(m.root.join("mod.py"), "x = 2").unwrap();
                        std::fs::remove_file(dir.join("mod.py")).unwrap();
                    }
                    Ok(m.root.clone())
                })
                .unwrap()
            };
            let first = run(true);
            assert!(!first.exists(), "the broken mirror was dropped");
            assert_ne!(run(false), first);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn hash_seed_is_pinned_unless_the_user_sets_one() {
        use crate::runner::{hash_seed_from, DEFAULT_HASH_SEED};
        use std::ffi::OsStr;
        assert_eq!(hash_seed_from(None), DEFAULT_HASH_SEED);
        assert_eq!(hash_seed_from(Some(OsStr::new(""))), DEFAULT_HASH_SEED);
        assert_eq!(hash_seed_from(Some(OsStr::new("123"))), "123");
        assert_eq!(hash_seed_from(Some(OsStr::new("random"))), "random");
    }

    #[test]
    fn sanitize_python_env_removes_caller_overrides() {
        let mut cmd = std::process::Command::new("python");
        cmd.env("PYTEST_ADDOPTS", "-n auto");
        cmd.env("PYTHONPATH", "/mirror");
        sanitize_python_env(&mut cmd);
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_owned())))
            .collect();
        // Every listed var is explicitly unset (`None`), not just left unset,
        // so it is stripped from the inherited parent environment too.
        for var in SANITIZED_ENV_VARS {
            assert_eq!(envs.get(*var), Some(&None), "{var} not removed");
        }
        // fermut's own pins are untouched.
        assert_eq!(
            envs.get("PYTHONPATH"),
            Some(&Some(std::ffi::OsString::from("/mirror")))
        );
        // Marked as a fermut child so `pytest --fermut` can't recurse.
        assert_eq!(
            envs.get(CHILD_ENV),
            Some(&Some(std::ffi::OsString::from("1")))
        );
    }

    #[test]
    fn baseline_status_displays_its_verdict() {
        use super::BaselineStatus;
        let wall = std::time::Duration::ZERO;
        assert_eq!(BaselineStatus::Passed { wall }.to_string(), "passed");
        let output = String::new();
        assert_eq!(BaselineStatus::Failed { output }.to_string(), "failed");
    }

    #[cfg(unix)]
    #[test]
    fn baseline_run_reports_pass_fail_timeout_and_missing_runner() {
        use super::BaselineStatus;
        use super::{process_group::with_new_process_group, run_baseline_with_timeout};
        use std::time::Duration;
        let sh = |script: &str| {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", script]);
            with_new_process_group(&mut c);
            c
        };
        let secs = Duration::from_secs;

        let ok = run_baseline_with_timeout(sh("exit 0"), secs(30)).unwrap();
        assert!(matches!(ok, BaselineStatus::Passed { .. }), "{ok:?}");

        let red = run_baseline_with_timeout(sh("echo out; echo err >&2; exit 1"), secs(30));
        match red.unwrap() {
            BaselineStatus::Failed { output } => assert_eq!(output, "out\nerr"),
            other => panic!("{other:?}"),
        }

        let hung = run_baseline_with_timeout(sh("echo started; sleep 30"), secs(1));
        match hung.unwrap() {
            BaselineStatus::Failed { output } => {
                assert!(
                    output.contains("exceeded --baseline-timeout (1s)"),
                    "{output}"
                );
                assert!(output.ends_with("started"), "{output}");
            }
            other => panic!("{other:?}"),
        }

        let missing = std::process::Command::new("fermut-no-such-runner");
        let err = run_baseline_with_timeout(missing, secs(30)).unwrap_err();
        assert!(
            err.to_string()
                .contains("`fermut-no-such-runner` was not found"),
            "{err}"
        );
    }

    #[test]
    fn tail_lines_keeps_all_when_under_limit() {
        let s = "a\nb\nc";
        assert_eq!(tail_lines(s, 40), "a\nb\nc");
    }

    #[test]
    fn tail_lines_truncates_and_marks_elision() {
        let s = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_lines(&s, 3);
        assert!(out.starts_with("… (7 earlier lines elided)\n"));
        assert!(out.ends_with("8\n9\n10"));
    }
    use crate::config::IsolationMode;
    use crate::mutator::{Mutant, Operator};
    use ruff_text_size::{TextRange, TextSize};
    use std::fs;

    fn make_mutant(file: std::path::PathBuf, start: u32, end: u32, repl: &str) -> Mutant {
        Mutant {
            id: format!("test-{start}-{end}"),
            file,
            operator: Operator::ArithOpSwap,
            range: TextRange::new(TextSize::from(start), TextSize::from(end)),
            original: "1".into(),
            replacement: repl.to_string(),
            line: 1,
            stmt_line: 1,
            site: None,
        }
    }

    #[test]
    fn copy_keeps_source_and_dotfiles_but_prunes_junk() {
        let src = tempfile::tempdir().unwrap();
        let s = src.path();

        // Source + config we must keep.
        fs::create_dir_all(s.join("src")).unwrap();
        fs::write(s.join("src/app.py"), "x = 1\n").unwrap();
        fs::write(s.join("pyproject.toml"), "[project]\n").unwrap();
        // Hidden *file* — tiny config that can affect tests; must be kept.
        fs::write(s.join(".env"), "K=v\n").unwrap();

        // Non-junk hidden dir — tests may run scripts from it; must be kept.
        fs::create_dir_all(s.join(".github/scripts")).unwrap();
        fs::write(s.join(".github/scripts/tool.py"), "x = 1\n").unwrap();

        // Named junk directories — the bulk of the junk; must be pruned.
        fs::create_dir_all(s.join(".git/objects")).unwrap();
        fs::write(s.join(".git/objects/big"), "junk").unwrap();
        fs::create_dir_all(s.join(".venv/lib")).unwrap();
        fs::write(s.join(".venv/lib/big"), "junk").unwrap();
        fs::create_dir_all(s.join(".claude")).unwrap();
        fs::write(s.join(".claude/state"), "junk").unwrap();

        // Gitignored dir — must be pruned (honored without a git checkout).
        fs::write(s.join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir_all(s.join("node_modules/pkg")).unwrap();
        fs::write(s.join("node_modules/pkg/index.js"), "junk").unwrap();

        // Compiled bytecode — must be pruned. A stale `.pyc` that still
        // validates against a patched `.py` would mask the mutation and falsely
        // report it survived (regression guard for the pyjwt false-survivor bug).
        fs::create_dir_all(s.join("src/__pycache__")).unwrap();
        fs::write(s.join("src/__pycache__/app.cpython-313.pyc"), "stale").unwrap();
        fs::write(s.join("src/legacy.pyc"), "stale").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let mirror = dst.path().join("mirror");
        copy_dir_all(s, &mirror, IsolationMode::Copy).unwrap();

        assert!(mirror.join("src/app.py").exists(), "source must be copied");
        assert!(
            !mirror.join("src/__pycache__").exists(),
            "__pycache__ must be pruned (stale bytecode masks mutations)"
        );
        assert!(
            !mirror.join("src/legacy.pyc").exists(),
            "stray .pyc must be pruned"
        );
        assert!(
            mirror.join("pyproject.toml").exists(),
            "config must be copied"
        );
        assert!(
            mirror.join(".env").exists(),
            "hidden config file must be kept"
        );
        assert!(
            mirror.join(".github/scripts/tool.py").exists(),
            "non-junk hidden dir must be kept"
        );
        assert!(!mirror.join(".git").exists(), "junk dir must be pruned");
        assert!(!mirror.join(".venv").exists(), "hidden dir must be pruned");
        assert!(
            !mirror.join(".claude").exists(),
            "hidden dir must be pruned"
        );
        assert!(
            !mirror.join("node_modules").exists(),
            "gitignored dir must be pruned"
        );
    }

    #[test]
    fn patch_guard_restores_original_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        fs::write(&file, b"x = 1\n").unwrap();

        {
            let _g = PatchGuard {
                path: file.clone(),
                original: Some(fs::read(&file).unwrap()),
            };
            fs::write(&file, b"x = 2\n").unwrap();
            assert_eq!(fs::read(&file).unwrap(), b"x = 2\n");
        }
        assert_eq!(fs::read(&file).unwrap(), b"x = 1\n");
    }

    #[test]
    fn patch_guard_restores_after_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        fs::write(&file, b"orig\n").unwrap();

        let file_for_thread = file.clone();
        let _ = std::panic::catch_unwind(move || {
            let _g = PatchGuard {
                path: file_for_thread.clone(),
                original: Some(fs::read(&file_for_thread).unwrap()),
            };
            fs::write(&file_for_thread, b"patched\n").unwrap();
            panic!("boom");
        });
        assert_eq!(fs::read(&file).unwrap(), b"orig\n");
    }

    /// Build a fake project: pyproject.toml + a source file + tests dir.
    /// Returns (project_root, tests_dir, source_file).
    fn fake_project() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        fs::write(root.join("pyproject.toml"), "[project]\n").unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        let src = root.join("app.py");
        fs::write(&src, b"x = 1\n").unwrap();
        let tests = root.join("tests");
        (tmp, root, tests, src)
    }

    #[test]
    fn hardlink_mode_preserves_source_when_patched() {
        let (_tmp, root, tests, src) = fake_project();
        let mirror = Mirror::build(&tests, IsolationMode::Hardlink).unwrap();
        let mirrored_src = mirror.root.join("app.py");
        // Hardlink should share inode with the source.
        assert!(mirrored_src.exists());

        let m = make_mutant(src.clone(), 4, 5, "2");
        let _guard = apply_patch(&mirror, &m).unwrap();

        // Patched file in mirror reflects the mutation.
        assert_eq!(fs::read(&mirrored_src).unwrap(), b"x = 2\n");
        // Source must NOT have been touched.
        assert_eq!(
            fs::read(&src).unwrap(),
            b"x = 1\n",
            "source must stay untouched under hardlink isolation"
        );

        // Guard drop restores patched file in mirror.
        drop(_guard);
        assert_eq!(fs::read(&mirrored_src).unwrap(), b"x = 1\n");
        assert_eq!(fs::read(&src).unwrap(), b"x = 1\n");
        // Avoid unused warning on root.
        let _ = root;
    }

    #[cfg(unix)]
    #[test]
    fn patch_through_symlinked_path_lands_in_mirror_not_source() {
        // macOS `/var` -> `/private/var`: the mutant's path and the mirror's
        // project root name the same tree through different prefixes. The
        // patch must still hit the mirror, never the real source.
        let (_tmp, _root, tests, src) = fake_project();
        let mirror = Mirror::build(&tests, IsolationMode::Copy).unwrap();
        let alias_dir = tempfile::tempdir().unwrap();
        let alias = alias_dir.path().join("alias");
        std::os::unix::fs::symlink(&mirror.project_root, &alias).unwrap();
        let mirrored_src = mirror.root.join("app.py");

        let m = make_mutant(alias.join("app.py"), 4, 5, "2");
        let guard = apply_patch(&mirror, &m).unwrap();
        assert_eq!(fs::read(&mirrored_src).unwrap(), b"x = 2\n");
        assert_eq!(
            fs::read(&src).unwrap(),
            b"x = 1\n",
            "source must stay untouched"
        );
        drop(guard);
        assert_eq!(fs::read(&mirrored_src).unwrap(), b"x = 1\n");
    }

    #[test]
    fn patch_outside_project_root_is_refused() {
        let (_tmp, _root, tests, _src) = fake_project();
        let mirror = Mirror::build(&tests, IsolationMode::Copy).unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("elsewhere.py");
        fs::write(&outside, b"x = 1\n").unwrap();

        let m = make_mutant(outside.clone(), 4, 5, "2");
        let err = apply_patch(&mirror, &m).err().expect("must refuse");
        assert!(format!("{err:#}").contains("outside the mirrored project root"));
        assert_eq!(fs::read(&outside).unwrap(), b"x = 1\n");
    }

    #[test]
    fn auto_mode_mirror_contents_match_source() {
        let (_tmp, _root, tests, src) = fake_project();
        let mirror = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let mirrored = mirror.root.join("app.py");
        assert_eq!(fs::read(&mirrored).unwrap(), fs::read(&src).unwrap());
    }

    /// Parallel pytest sessions sharing one temp root race in its cleanup, a
    /// false kill under `filterwarnings = error`; each mirror gets its own.
    #[test]
    fn mirror_commands_get_a_private_pytest_temp_root() {
        let (_tmp, _root, tests, _src) = fake_project();
        let a = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let b = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let mut cmd = std::process::Command::new("true");
        crate::runner::configure_mirror_cmd(&mut cmd, &a, true).unwrap();
        let env: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        let root = env[std::ffi::OsStr::new("PYTEST_DEBUG_TEMPROOT")].unwrap();
        assert_eq!(std::path::Path::new(root), a.pytest_temproot);
        assert!(a.pytest_temproot.is_dir(), "pytest needs the root to exist");
        assert!(
            !a.pytest_temproot.starts_with(&a.root),
            "outside the project copy"
        );
        assert_ne!(a.pytest_temproot, b.pytest_temproot);
        let seed = env[std::ffi::OsStr::new("PYTHONHASHSEED")].unwrap();
        assert_eq!(
            seed,
            crate::runner::hash_seed().as_str(),
            "every run, one order"
        );
    }

    /// Tests creating app dirs under `~` race across workers (typer's
    /// `get_app_dir`); each mirror gets its own home unless opted out.
    #[test]
    fn mirror_commands_get_a_private_home_unless_opted_out() {
        let (_tmp, _root, tests, _src) = fake_project();
        let a = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let b = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let env_of = |m: &Mirror, private: bool| {
            let mut cmd = std::process::Command::new("true");
            crate::runner::configure_mirror_cmd(&mut cmd, m, private).unwrap();
            cmd.get_envs()
                .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let var = |env: &std::collections::HashMap<_, Option<std::ffi::OsString>>, k: &str| {
            env.get(std::ffi::OsStr::new(k)).cloned().flatten()
        };
        let on = env_of(&a, true);
        assert_eq!(var(&on, "HOME"), Some(a.home.clone().into()));
        assert!(a.home.is_dir() && !a.home.starts_with(&a.root));
        assert_eq!(
            var(&on, "XDG_CONFIG_HOME"),
            Some(a.home.join(".config").into())
        );
        assert_eq!(
            var(&on, "XDG_STATE_HOME"),
            Some(a.home.join(".local/state").into())
        );
        assert_eq!(
            var(&on, crate::runner::PRIVATE_HOME_ENV).as_deref(),
            Some(std::ffi::OsStr::new("1"))
        );
        assert_ne!(var(&on, "HOME"), var(&env_of(&b, true), "HOME"));
        let off = env_of(&a, false);
        assert_eq!(var(&off, "HOME"), None, "inherits the real home");
        assert_eq!(var(&off, crate::runner::PRIVATE_HOME_ENV), None);
    }

    #[test]
    fn db_suffix_is_per_worker_after_the_users_own() {
        use super::db_suffix_from as f;
        assert_eq!(f(None, Some(2)).as_deref(), Some("fm2"));
        assert_eq!(f(Some("py313"), Some(0)).as_deref(), Some("py313_fm0"));
        assert_eq!(f(Some(""), Some(1)).as_deref(), Some("fm1"));
        assert_eq!(f(Some("py313"), None).as_deref(), Some("py313"));
        assert_eq!(f(None, None), None, "baseline: as a cold run");
    }

    /// Windows moves `USERPROFILE` and the AppData dirs too; elsewhere they
    /// stay untouched.
    #[test]
    fn set_home_moves_windows_dirs_only_on_windows() {
        let home = std::path::Path::new("/h");
        let env_of = |windows: bool| {
            let mut cmd = std::process::Command::new("true");
            super::set_home(&mut cmd, home, windows);
            cmd.get_envs()
                .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let win = env_of(true);
        let get = |k: &str| win.get(std::ffi::OsStr::new(k)).cloned().flatten();
        assert_eq!(get("HOME"), Some(home.into()));
        assert_eq!(get("USERPROFILE"), Some(home.into()));
        assert_eq!(get("APPDATA"), Some(home.join(r"AppData\Roaming").into()));
        assert_eq!(
            get("LOCALAPPDATA"),
            Some(home.join(r"AppData\Local").into())
        );
        let unix = env_of(false);
        for k in ["USERPROFILE", "APPDATA", "LOCALAPPDATA"] {
            assert!(!unix.contains_key(std::ffi::OsStr::new(k)), "{k} set");
        }
    }

    /// Python on Windows takes `~` from `USERPROFILE`, not `HOME`; the
    /// private home must still be where `Path.home()` lands.
    #[cfg(windows)]
    #[test]
    fn python_home_resolves_into_the_private_home_on_windows() {
        let Ok(py) = which::which("python") else {
            eprintln!("skipping: no python on PATH");
            return;
        };
        let (_tmp, _root, tests, _src) = fake_project();
        let m = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let mut cmd = std::process::Command::new(py);
        cmd.args([
            "-c",
            "import os, pathlib; print(pathlib.Path.home()); print(os.path.expanduser('~'))",
        ]);
        crate::runner::configure_mirror_cmd(&mut cmd, &m, true).unwrap();
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let home = m.home.canonicalize().unwrap();
        for line in String::from_utf8(out.stdout).unwrap().lines() {
            assert_eq!(
                std::path::Path::new(line).canonicalize().unwrap(),
                home,
                "`~` escaped the private home"
            );
        }
    }

    /// werkzeug parametrizes on absolute paths, so the recorded id carries the
    /// project's path; in the mirror pytest names the same test with the
    /// mirror's path. Ids go in rewritten and results come back mapped.
    #[test]
    fn node_ids_with_an_embedded_project_path_round_trip_through_the_mirror() {
        let (_tmp, root, tests, _src) = fake_project();
        let mirror = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let real = root.canonicalize().unwrap();
        let recorded = format!(
            "tests/test_f.py::test_dir[{}/tests/res-a.txt]",
            super::id_path(&real)
        );
        let in_mirror = mirror.id_into_mirror(&recorded);
        assert_eq!(
            in_mirror,
            format!(
                "tests/test_f.py::test_dir[{}/tests/res-a.txt]",
                super::id_path(&mirror.root)
            )
        );
        assert_eq!(
            mirror.text_from_mirror(&format!("FAILED {in_mirror} - boom")),
            format!("FAILED {recorded} - boom")
        );
        // Ids without the path, and the file part, are left alone.
        assert_eq!(
            mirror.id_into_mirror("tests/test_f.py::test_x[1]"),
            "tests/test_f.py::test_x[1]"
        );
        assert_eq!(mirror.id_into_mirror("-x"), "-x");
        // A sibling directory sharing the prefix is not the project.
        let sibling = format!(
            "tests/test_f.py::test_dir[{}2/a.txt]",
            super::id_path(&real)
        );
        assert_eq!(mirror.id_into_mirror(&sibling), sibling);
    }

    /// Only whole paths are mapped: not a sibling sharing the prefix, not a
    /// relative path that happens to contain it.
    #[test]
    fn replace_path_maps_whole_paths_only() {
        let r = |t: &str| replace_path(t, "/app", "/m");
        assert_eq!(r("t::k[/app/x]"), "t::k[/m/x]");
        assert_eq!(r("t::k[1-/app/x]"), "t::k[1-/m/x]");
        assert_eq!(r("/app"), "/m");
        assert_eq!(
            r("t::k[fixtures/app/cfg.json]"),
            "t::k[fixtures/app/cfg.json]"
        );
        assert_eq!(r("t::k[/app2/x]"), "t::k[/app2/x]");
        assert_eq!(r("t::k[x/app]"), "t::k[x/app]");
    }

    #[test]
    fn shown_path_drops_the_windows_verbatim_prefix() {
        let s = |p: &str| shown_path(std::path::Path::new(p));
        assert_eq!(s(r"\\?\C:\proj"), r"C:\proj");
        assert_eq!(s(r"\\?\UNC\host\share\proj"), r"\\host\share\proj");
        assert_eq!(s("/private/var/proj"), "/private/var/proj");
    }

    /// Node ids carry string params `unicode_escape`d, so the mapped paths
    /// must be too (Windows separators, non-ASCII home dirs).
    #[test]
    fn id_paths_are_escaped_like_pytest_ids() {
        assert_eq!(py_unicode_escape(r"C:\proj"), r"C:\\proj");
        assert_eq!(py_unicode_escape("/Users/José"), r"/Users/Jos\xe9");
        assert_eq!(py_unicode_escape("a\tΩ😀"), r"a\t\u03a9\U0001f600");
        assert_eq!(py_unicode_escape("a\r\nb"), r"a\r\nb");
        assert_eq!(py_unicode_escape("/repo/x y'\""), "/repo/x y'\"");
        // Width switches at U+0100 and U+10000.
        assert_eq!(
            py_unicode_escape("\u{ff}\u{100}\u{ffff}\u{10000}"),
            r"\xff\u0100\uffff\U00010000"
        );
    }

    /// pytest prints test ids relative to its resolved cwd, so a mirror path
    /// behind a symlink (macOS `/var`) would leak the temp dir into every id.
    #[test]
    fn mirror_paths_are_resolved() {
        let (_tmp, _root, tests, _src) = fake_project();
        let mirror = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        assert_eq!(mirror.root, mirror.root.canonicalize().unwrap());
        assert!(mirror.tests.starts_with(&mirror.root));
    }

    #[test]
    fn mirror_copies_gitignored_native_extensions_but_not_ignored_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("pkg")).unwrap();
        fs::create_dir_all(src.join("venv/lib")).unwrap();
        fs::write(src.join(".gitignore"), "*.so\nvenv/\n").unwrap();
        fs::write(src.join("pkg/_ext.cpython-313-darwin.so"), "elf").unwrap();
        fs::write(src.join("venv/lib/dep.so"), "elf").unwrap();
        let dst = tmp.path().join("dst");
        copy_dir_all(&src, &dst, IsolationMode::Copy).unwrap();
        assert!(dst.join("pkg/_ext.cpython-313-darwin.so").is_file());
        assert!(!dst.join("venv").exists());
    }

    /// flaskbb compiles `*.mo` catalogues into its package and gitignores
    /// them; without them its translation test fails in the mirror only.
    #[test]
    fn mirror_copies_gitignored_files_inside_packages_only() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        for d in ["pkg/tr/de/LC_MESSAGES", "pkg/build", "tests"] {
            fs::create_dir_all(src.join(d)).unwrap();
        }
        fs::write(src.join(".gitignore"), "*.mo\n*.log\n*.pyc\nbuild/\n").unwrap();
        fs::write(src.join("pkg/__init__.py"), "").unwrap();
        fs::write(src.join("pkg/tr/de/LC_MESSAGES/m.po"), "po").unwrap();
        fs::write(src.join("pkg/tr/de/LC_MESSAGES/m.mo"), "mo").unwrap();
        fs::write(src.join("pkg/stale.pyc"), "pyc").unwrap();
        fs::write(src.join("pkg/build/out.mo"), "mo").unwrap();
        fs::write(src.join("run.log"), "log").unwrap();
        fs::write(src.join("tests/data.mo"), "mo").unwrap();
        let dst = tmp.path().join("dst");
        copy_dir_all(&src, &dst, IsolationMode::Copy).unwrap();
        assert!(dst.join("pkg/tr/de/LC_MESSAGES/m.mo").is_file());
        assert!(dst.join("pkg/tr/de/LC_MESSAGES/m.po").is_file());
        assert!(!dst.join("pkg/stale.pyc").exists());
        assert!(!dst.join("pkg/build").exists(), "ignored dir stays pruned");
        assert!(!dst.join("run.log").exists(), "root-level ignored file");
        assert!(!dst.join("tests/data.mo").exists(), "not inside a package");
    }

    #[test]
    fn baseline_exit_status_decides_passed_or_failed() {
        use super::{run_baseline_with_timeout, BaselineStatus};
        let py = super::interpreter(None);
        let run = |code: &str| {
            let mut cmd = std::process::Command::new(&py);
            cmd.args(["-c", code]);
            run_baseline_with_timeout(cmd, std::time::Duration::from_secs(60)).unwrap()
        };
        if std::process::Command::new(&py)
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no Python on this box
        }
        assert!(matches!(run("pass"), BaselineStatus::Passed { .. }));
        match run("import sys; print('boom'); sys.exit(3)") {
            BaselineStatus::Failed { output } => assert!(output.contains("boom"), "{output}"),
            BaselineStatus::Passed { .. } => panic!("exit 3 must fail the baseline"),
        }
    }

    /// A baseline that ran without the `__pycache__` the mutant mirrors get
    /// would pass a package-listing test that every mutant run then fails.
    #[test]
    fn baseline_mirror_is_precompiled_like_worker_mirrors() {
        let py = super::interpreter(None);
        if std::process::Command::new(&py)
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no Python on this box
        }
        let (_tmp, root, tests, _src) = fake_project();
        fs::write(root.join("m.py"), "X = 1\n").unwrap();
        let mirror = super::build_baseline_mirror(&tests, IsolationMode::Copy, Some(&py)).unwrap();
        assert!(mirror.root.join("__pycache__").is_dir());
    }

    /// The stale-`.pyc` false survivor, against a precompiled mirror: a
    /// same-size mutation with the original mtime would still validate a
    /// timestamp `.pyc`, so Python would run the original bytecode. The
    /// checked-hash `.pyc` from `precompile` must miss and compile the patch.
    #[test]
    fn precompiled_mirror_still_applies_same_size_same_mtime_mutation() {
        let py = super::interpreter(None);
        if std::process::Command::new(&py)
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no Python on this box
        }
        let (_tmp, root, tests, _src) = fake_project();
        let module = root.join("m.py");
        fs::write(&module, "def f(a, b):\n    return a < b\n").unwrap();
        let mirror = Mirror::build(&tests, IsolationMode::Copy).unwrap();
        super::precompile(&mirror.root, &py);

        let pyc = fs::read_dir(mirror.root.join("__pycache__"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("m."))
            .expect("precompile wrote m's .pyc");
        let header = fs::read(&pyc).unwrap();
        assert_eq!(header[4..8], [3, 0, 0, 0], "flags must be checked-hash");

        let target = mirror.root.join("m.py");
        let mtime = fs::metadata(&target).unwrap().modified().unwrap();
        let f_of_1_2 = || {
            std::process::Command::new(&py)
                .args(["-c", "import sys, m; sys.exit(0 if m.f(1, 2) else 1)"])
                .current_dir(&mirror.root)
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .status()
                .unwrap()
                .code()
        };
        {
            let _guard = apply_patch(&mirror, &make_mutant(module, 26, 27, ">")).unwrap();
            fs::File::options()
                .write(true)
                .open(&target)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            assert_eq!(
                f_of_1_2(),
                Some(1),
                "mutation must apply, not the cached original"
            );
        }
        assert_eq!(f_of_1_2(), Some(0), "restored original runs from its .pyc");
    }

    /// The trait defaults are what test doubles and the unittest runner get:
    /// no deselection, no learned kills or kill-sets, no single-test audit.
    #[test]
    fn runner_trait_defaults_are_inert() {
        use super::{BaselineStatus, Runner};
        use crate::report::MutantOutcome;
        struct Bare;
        impl Runner for Bare {
            fn run(&self, m: &std::sync::Arc<Mutant>) -> anyhow::Result<MutantOutcome> {
                Ok(MutantOutcome::killed(m.clone()))
            }
            fn baseline(&self) -> anyhow::Result<BaselineStatus> {
                Ok(BaselineStatus::Passed {
                    wall: std::time::Duration::ZERO,
                })
            }
        }
        let mut r = Bare;
        let m = std::sync::Arc::new(make_mutant("a.py".into(), 0, 1, "y"));
        r.prepare(std::slice::from_ref(&m)).unwrap();
        r.set_timeout(std::time::Duration::from_secs(1));
        r.set_memory_cap(1 << 20);
        assert_eq!(r.baseline_deselection(), None);
        assert!(r.take_kill_records().is_empty());
        assert!(r.take_kill_sets().is_empty());
        assert!(r.audit(&m, "tests/test_a.py::test_a").unwrap().is_none());
    }

    #[test]
    fn project_root_stops_at_any_marker_before_an_outer_pyproject() {
        // #76: a Django app with only `pytest.ini` (or tox.ini / setup.py /
        // a worktree's `.git` file) under a dir holding `pyproject.toml` used
        // to resolve to the outer dir and mirror the wrong tree.
        for marker in ["pytest.ini", ".pytest.ini", "tox.ini", "setup.py", ".git"] {
            let tmp = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
            let app = tmp.path().join("app");
            fs::create_dir_all(app.join("tests")).unwrap();
            fs::write(app.join(marker), "").unwrap();
            assert_eq!(
                super::find_project_root(&app.join("tests")),
                Some(app),
                "{marker}"
            );
        }
    }

    #[test]
    fn mirror_without_project_marker_roots_at_the_tests_parent() {
        // No project marker anywhere up the temp path: the tests dir's
        // parent is the project.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("app.py"), "x = 1\n").unwrap();
        if super::find_project_root(&root).is_some() {
            return; // a marker above the temp dir; nothing to test here
        }
        let mirror = Mirror::build(&root.join("tests"), IsolationMode::Copy).unwrap();
        assert_eq!(fs::read(mirror.root.join("app.py")).unwrap(), b"x = 1\n");
        assert!(mirror.tests.ends_with("tests"));
    }

    #[test]
    fn time_mirror_build_builds_and_drops_the_mirror() {
        let (_tmp, _root, tests, _src) = fake_project();
        let elapsed = super::time_mirror_build(&tests, IsolationMode::Copy).unwrap();
        assert!(elapsed > std::time::Duration::ZERO);
    }

    #[test]
    fn patch_guard_restore_failure_does_not_panic() {
        // The mirror file's directory is gone: both restore attempts fail and
        // drop only logs (a panic in drop during unwinding would abort).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gone/app.py");
        drop(PatchGuard {
            path: path.clone(),
            original: Some(b"x = 1\n".to_vec()),
        });
        assert!(!path.exists());
    }

    #[test]
    fn precompile_is_best_effort() {
        let (_tmp, root, _tests, _src) = fake_project();
        // An interpreter that exits non-zero, and one that can't be spawned.
        super::precompile(&root, std::path::Path::new("false"));
        super::precompile(&root, &root.join("no-such-python"));
        assert!(!root.join("__pycache__").exists());
    }

    #[test]
    fn clone_file_error_names_both_paths_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("missing.py");
        let dst = tmp.path().join("out.py");
        for mode in [IsolationMode::Copy, IsolationMode::Hardlink] {
            let err = super::clone_file(&src, &dst, mode).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("missing.py -> "), "{msg}");
            assert!(msg.contains(&format!("(mode={mode})")), "{msg}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn baseline_spawn_failure_other_than_not_found_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("not-executable");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        let err = super::run_baseline_with_timeout(
            std::process::Command::new(&script),
            std::time::Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("spawning baseline test process"),
            "{err}"
        );
    }

    #[test]
    fn mirror_without_project_root_uses_the_tests_parent() {
        // No pyproject.toml/setup.cfg anywhere above: the tests dir's parent
        // is the project root.
        let tmp = tempfile::tempdir().unwrap();
        let tests = tmp.path().join("tests");
        fs::create_dir_all(&tests).unwrap();
        fs::write(tmp.path().join("app.py"), "x = 1\n").unwrap();
        if super::find_project_root(&tests).is_some() {
            return; // a project marker above the temp dir; can't test here
        }
        let mirror = Mirror::build(&tests, IsolationMode::Copy).unwrap();
        assert!(mirror.root.join("app.py").is_file());
        assert!(super::time_mirror_build(&tests, IsolationMode::Copy).is_ok());
    }

    #[test]
    fn patch_guard_without_original_and_with_unwritable_path_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("missing-dir").join("a.py");
        // Nothing to restore.
        drop(PatchGuard {
            path: gone.clone(),
            original: None,
        });
        // Both restore attempts fail (parent dir missing): logged, not raised.
        drop(PatchGuard {
            path: gone.clone(),
            original: Some(b"x = 1\n".to_vec()),
        });
        assert!(!gone.exists());
    }

    #[test]
    fn precompile_failure_is_best_effort() {
        let tmp = tempfile::tempdir().unwrap();
        // `false` runs but exits non-zero: only costs the speedup.
        super::precompile(tmp.path(), std::path::Path::new("false"));
        assert!(!tmp.path().join("__pycache__").exists());
    }

    #[test]
    fn copy_dir_all_reports_a_file_it_cannot_materialize() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.py"), "x = 1\n").unwrap();
        // A directory squatting on the target path makes the copy fail while
        // the source still exists: that's a real error, not a mid-walk delete.
        fs::create_dir_all(dst.path().join("a.py")).unwrap();
        let err = copy_dir_all(src.path(), dst.path(), IsolationMode::Copy).unwrap_err();
        assert!(format!("{err:#}").contains("materializing"), "{err:#}");
    }
}
