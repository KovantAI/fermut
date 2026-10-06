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
pub(crate) mod process_group;
pub mod pytest;
pub mod python;
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

    /// Drain the kills learned this run (which test killed which
    /// `(file, operator)`), for the engine to fold into the smart-ordering
    /// sidecar. Default empty — only the pytest runner learns them, and only
    /// when smart ordering is on.
    fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
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

/// Result of the pre-flight baseline run (the unmutated suite).
#[derive(Debug)]
pub enum BaselineStatus {
    /// The suite exited 0 — safe to proceed with mutation.
    Passed,
    /// The suite failed/errored. `output` is the tail of pytest's combined
    /// stdout/stderr, for surfacing in the abort message.
    Failed { output: String },
}

impl std::fmt::Display for BaselineStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BaselineStatus::Passed => "passed",
            BaselineStatus::Failed { .. } => "failed",
        })
    }
}

/// Build a one-shot mirror for a baseline run. Unlike [`with_worker_mirror`],
/// this isn't cached in a thread-local — the baseline runs once on the calling
/// thread before the rayon pool spins up, so there is nothing to reuse.
pub(crate) fn build_mirror(tests: &Path, mode: IsolationMode) -> Result<Mirror> {
    Mirror::build(tests, mode)
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

/// Strip [`SANITIZED_ENV_VARS`] from `cmd` so every suite run is reproducible
/// from the project alone, independent of the caller's shell.
pub(crate) fn sanitize_python_env(cmd: &mut std::process::Command) {
    for var in SANITIZED_ENV_VARS {
        cmd.env_remove(var);
    }
}

/// Apply the environment every per-mutant framework command against a mirror
/// needs: run from the mirror root, in a fresh process group (so a timeout kill
/// reaches child processes), with `PYTHONPATH` pinned to the mirror (beats an
/// editable install's `.pth` pointing at the original tree) and bytecode writes
/// disabled (a stale `.pyc` that still validates against the patched source
/// would mask the mutation and falsely report a survivor). Shared by the pytest
/// and unittest runners so these correctness-critical knobs can't drift apart.
pub(crate) fn configure_mirror_cmd(cmd: &mut std::process::Command, mirror: &Mirror) -> Result<()> {
    cmd.current_dir(&mirror.root);
    process_group::with_new_process_group(cmd);
    cmd.env("PYTHONPATH", mirror_pythonpath(mirror)?);
    cmd.env("PYTHONDONTWRITEBYTECODE", "1");
    sanitize_python_env(cmd);
    Ok(())
}

/// Run `f` against this worker's mirror with `mutant`'s patch spliced in for the
/// duration of the call: reuse (or lazily build) the thread-local mirror, apply
/// the patch, invoke `f`, then revert the patch on scope exit via the guard.
/// Both runners funnel through here so the mirror-reuse + patch-lifecycle wiring
/// lives in exactly one place.
pub(crate) fn run_patched<F>(
    tests: &Path,
    mode: IsolationMode,
    mutant: &Mutant,
    f: F,
) -> Result<MutantOutcome>
where
    F: FnOnce(&Mirror) -> Result<MutantOutcome>,
{
    with_worker_mirror(tests, mode, |mirror| {
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
                 Install it and activate your virtualenv if you use one, then verify with \
                 `fermut doctor`:\n  \
                 pytest  → `pip install pytest pytest-cov`\n  \
                 unittest → ensure `python` is on PATH"
            );
        }
        Err(e) => return Err(e).context("spawning baseline test process"),
    };

    let read_tail = || -> String {
        let mut s = std::fs::read_to_string(out_file.path()).unwrap_or_default();
        s.push_str(&std::fs::read_to_string(err_file.path()).unwrap_or_default());
        tail_lines(&s, 40)
    };

    match child
        .wait_timeout(timeout)
        .context("waiting on baseline run")?
    {
        Some(status) if status.success() => Ok(BaselineStatus::Passed),
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
    let key_base = cfg.source_root.clone();
    // `rstest` is a pytest-CLI-compatible drop-in, so it reuses the pytest
    // runner wholesale — only the framework executable name differs.
    let pytest_compatible = |exe: &'static str| -> Box<dyn Runner> {
        Box::new(pytest::PytestRunner::new(pytest::PytestConfig {
            tests: tests.clone(),
            timeout,
            baseline_timeout,
            hypothesis_seed: cfg.hypothesis_seed,
            extra_args: cfg.pytest_args.clone(),
            isolation,
            coverage: cfg.coverage.clone(),
            python: cfg.python.clone(),
            exe,
            smart_order: cfg.smart_order,
            kill_order: kill_order.clone(),
            kill_sink: kill_sink.clone(),
            // Only coverage-selected runs can name a killer the cache can use.
            learn_killer: cfg.cache && cfg.coverage.is_some(),
            key_base: key_base.clone(),
        }))
    };
    match cfg.runner {
        RunnerKind::Pytest => pytest_compatible("pytest"),
        RunnerKind::Rstest => pytest_compatible("rstest"),
        RunnerKind::Unittest => Box::new(unittest::UnittestRunner::new(
            tests,
            timeout,
            baseline_timeout,
            isolation,
            // Resolve the interpreter once: the configured `--python` when set,
            // else a PATH-probed `python3`/`python`. `unittest` has no console
            // script, so it's always `<interp> -m unittest`.
            interpreter(cfg.python.as_deref()),
            cfg.unittest_pattern.clone(),
        )),
    }
}

/// Per-worker mirror of the project tree. Built once on first use, then
/// reused for every mutant that worker handles.
pub(crate) struct Mirror {
    _workdir: tempfile::TempDir,
    pub root: PathBuf,
    pub tests: PathBuf,
    pub project_root: PathBuf,
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
    fn build(tests: &Path, mode: IsolationMode) -> Result<Self> {
        let workdir = tempfile::tempdir().context("creating workdir")?;
        let project_root = find_project_root(tests).unwrap_or_else(|| {
            tests
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| tests.to_path_buf())
        });
        let root = workdir.path().join("project");
        copy_dir_all(&project_root, &root, mode)?;
        let tests_rel = tests.strip_prefix(&project_root).unwrap_or(tests);
        let tests_in_mirror = root.join(tests_rel);
        Ok(Self {
            _workdir: workdir,
            root,
            tests: tests_in_mirror,
            project_root,
        })
    }
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
    let rel = mutant
        .file
        .strip_prefix(&mirror.project_root)
        .unwrap_or(&mutant.file);
    let target = mirror.root.join(rel);

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
    static WORKER_MIRROR: RefCell<Option<(PathBuf, IsolationMode, Mirror)>> =
        const { RefCell::new(None) };
}

/// Run `f` against this worker's mirror, building one on first call.
/// The mirror is rebuilt only if `tests` or isolation mode changes — neither
/// should within a run.
pub(crate) fn with_worker_mirror<F>(
    tests: &Path,
    mode: IsolationMode,
    f: F,
) -> Result<MutantOutcome>
where
    F: FnOnce(&Mirror) -> Result<MutantOutcome>,
{
    WORKER_MIRROR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let needs_build = !matches!(&*slot, Some((p, m, _)) if p == tests && *m == mode);
        if needs_build {
            *slot = None;
            let mirror = Mirror::build(tests, mode)?;
            *slot = Some((tests.to_path_buf(), mode, mirror));
        }
        let mirror = &slot.as_ref().unwrap().2;
        f(mirror)
    })
}

pub(crate) fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut p = start;
    loop {
        if p.join("pyproject.toml").exists() || p.join("setup.cfg").exists() {
            return Some(p.to_path_buf());
        }
        p = p.parent()?;
    }
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
    //   - hidden *directories* (`.git`, `.venv`, `.claude`, `.pytest_cache`, ...) —
    //     the bulk of the junk, via the `filter_entry` predicate below.
    //   - `__pycache__` directories and stray `.pyc`/`.pyo` files. CRITICAL for
    //     correctness, not just size: the mirror is reused across mutants, and a
    //     stale compiled `.pyc` whose (mtime, size) still validates against the
    //     patched `.py` makes Python import the ORIGINAL bytecode — the mutation
    //     silently doesn't apply and the mutant is falsely reported survived.
    //     Same-size mutations (e.g. `<`→`>`) are the worst case since the size
    //     check can't catch them. We also set PYTHONDONTWRITEBYTECODE on the test
    //     subprocess so pytest can't write fresh `.pyc` back mid-run.
    //   - anything matched by `.gitignore` (`venv`, `node_modules`, data dirs, ...).
    //
    // What we keep: hidden *files*. Dotfiles like `.env`, `.python-version`,
    // `.coveragerc`, `.flake8` are tiny but can change test behavior, so `hidden`
    // filtering stays off and we only drop hidden directories. `require_git(false)`
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
            // Drop hidden directories and `__pycache__` (stale bytecode).
            !(is_dir && name.is_some_and(|n| n.starts_with('.') || n == "__pycache__"))
        })
        .build();
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        if path == src {
            continue;
        }
        let rel = path.strip_prefix(src).unwrap();
        let target = dst.join(rel);
        let file_type = entry
            .file_type()
            .with_context(|| format!("no file type for {}", path.display()))?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("create mirror dir {}", target.display()))?;
        } else if file_type.is_file() {
            // Skip stray compiled bytecode outside `__pycache__` (e.g. legacy
            // sidecar `.pyc`); same staleness hazard as above.
            if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("pyc") | Some("pyo")
            ) {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create mirror dir {}", parent.display()))?;
            }
            clone_file(path, &target, mode)?;
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
        apply_patch, copy_dir_all, sanitize_python_env, tail_lines, Mirror, PatchGuard,
        SANITIZED_ENV_VARS,
    };

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

        // Hidden *directories* — the bulk of the junk; must be pruned.
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

    #[test]
    fn auto_mode_mirror_contents_match_source() {
        let (_tmp, _root, tests, src) = fake_project();
        let mirror = Mirror::build(&tests, IsolationMode::Auto).unwrap();
        let mirrored = mirror.root.join("app.py");
        assert_eq!(fs::read(&mirrored).unwrap(), fs::read(&src).unwrap());
    }
}
