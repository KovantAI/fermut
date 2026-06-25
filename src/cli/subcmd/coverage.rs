//! `fermut coverage` — generate / refresh the `.coverage` SQLite database
//! that drives per-mutant test selection, without making the user remember
//! the pytest-cov incantation.
//!
//! The whole point is to be the one command a user runs after touching their
//! tests. Zero-arg `fermut coverage`:
//!
//! 1. Discovers source + tests from `fermut.toml` / `pyproject.toml` (same
//!    walk as `run`).
//! 2. If no `.coverage` exists yet, runs the full suite under coverage.
//! 3. If one exists, runs only the test files changed since the DB was
//!    written, appending into it — so adding a test costs one test file's
//!    runtime, not the whole suite.
//!
//! fermut reads the `.coverage` SQLite directly (see
//! [`crate::filter::coverage`]), so there is no `coverage json` export step.
//!
//! ## Incremental correctness
//!
//! `pytest --cov-append` *unions* coverage data per context. For a brand-new
//! test that is exactly right. For a *modified* test that now executes fewer
//! lines, the old line bits would linger and falsely mark a line as covered.
//! So before appending we DELETE the changed test files' contexts from the DB
//! (`purge_contexts`), guaranteeing their line bits are rebuilt from scratch.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::config::loader::LoadedConfig;

#[derive(Debug, Clone)]
pub struct CoverageOpts {
    pub path: PathBuf,
    /// Override the measured source root (`--cov=<this>`). Defaults to the
    /// configured `source_root`, else the project root.
    pub source: Option<PathBuf>,
    /// Override the tests location. Defaults to configured `tests`, else
    /// `<source>/tests`.
    pub tests: Option<PathBuf>,
    /// Force a full-suite run even when an up-to-date `.coverage` exists.
    pub full: bool,
    /// Where to write the database. Defaults to `<project>/.coverage`.
    pub output: Option<PathBuf>,
    /// Interpreter (path) or virtualenv (dir) to run pytest with — same as
    /// `fermut run --python`. When set, fermut invokes `<python> -m pytest`,
    /// so coverage generation needs no `pytest` on PATH (restricted
    /// sandboxes/CI). Auto-discovers a venv when omitted.
    pub python: Option<PathBuf>,
    /// Extra args forwarded to pytest (repeatable).
    pub pytest_args: Vec<String>,
}

pub fn coverage(opts: CoverageOpts) -> Result<()> {
    let loaded = LoadedConfig::load(&opts.path)?;
    let base_dir = loaded.base_dir.clone();

    let source = opts
        .source
        .or_else(|| {
            loaded
                .file
                .source_root
                .clone()
                .map(|p| loaded.resolve_path(p))
        })
        .unwrap_or_else(|| base_dir.clone());

    let tests = opts
        .tests
        .or_else(|| loaded.file.tests.clone().map(|p| loaded.resolve_path(p)))
        .unwrap_or_else(|| source.join("tests"));

    let db_path = opts
        .output
        .clone()
        .unwrap_or_else(|| base_dir.join(".coverage"));

    // Resolve the interpreter the same way `fermut run` does: explicit
    // `--python` (relative anchored at the config base), else auto-discover a
    // venv near the source root. `None` keeps the bare-`pytest`-on-PATH path.
    let python = {
        let explicit = opts.python.clone().map(|p| loaded.resolve_path(p));
        crate::runner::resolve_python(&source, explicit.as_deref())
    };

    // pytest-cov takes `--cov=<path-or-module>`; a path relative to the run
    // dir keeps recorded file keys short and stable. Fall back to the
    // absolute path when source lives outside the project root. An empty
    // relative path (source IS the project root) must become `.` — pytest-cov
    // treats `--cov=` with an empty value as "measure nothing".
    let cov_target = match rel_to(&source, &base_dir) {
        Some(p) if p.as_os_str().is_empty() => PathBuf::from("."),
        Some(p) => p,
        None => source.clone(),
    };

    let full = opts.full || !db_path.exists();

    if full {
        println!("Running full test suite under coverage…");
        run_pytest(
            &base_dir,
            &cov_target,
            std::slice::from_ref(&tests),
            false,
            &opts.pytest_args,
            python.as_deref(),
        )?;
        report_done(&loaded, &db_path, None);
        return Ok(());
    }

    // Incremental: which test files changed since the DB was last written?
    let db_mtime = std::fs::metadata(&db_path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let changed = changed_test_files(&tests, db_mtime);

    if changed.is_empty() {
        println!(
            "{} is up to date (no test files changed since it was written). \
             Nothing to do.",
            display_rel(&db_path, &base_dir)
        );
        return Ok(());
    }

    println!(
        "{} changed test file(s) since last coverage; appending:",
        changed.len()
    );
    for f in &changed {
        println!("  {}", display_rel(f, &base_dir));
    }

    // Drop the changed files' stale contexts so --cov-append rebuilds them
    // cleanly (a modified test covering fewer lines must not keep old bits).
    let prefixes: Vec<String> = changed
        .iter()
        .filter_map(|f| rel_to(f, &base_dir))
        .filter_map(|p| p.to_str().map(|s| s.replace('\\', "/")))
        .collect();
    let purged = purge_contexts(&db_path, &prefixes)
        .with_context(|| format!("purging stale contexts from {}", db_path.display()))?;
    if purged > 0 {
        println!("Purged {purged} stale context(s) for changed files.");
    }

    run_pytest(
        &base_dir,
        &cov_target,
        &changed,
        true,
        &opts.pytest_args,
        python.as_deref(),
    )?;
    report_done(&loaded, &db_path, Some(changed.len()));
    Ok(())
}

/// Invoke `pytest <targets> --cov=<target> --cov-context=test [--cov-append]`
/// from `base_dir`, inheriting stdio so the user sees the live test run.
fn run_pytest(
    base_dir: &Path,
    cov_target: &Path,
    targets: &[PathBuf],
    append: bool,
    extra: &[String],
    python: Option<&Path>,
) -> Result<()> {
    // With an interpreter, go through `<python> -m pytest` so coverage runs
    // on that interpreter's pytest-cov with no reliance on PATH; otherwise a
    // bare `pytest` console script (historical behavior).
    let mut cmd = match python {
        Some(py) => {
            let mut c = Command::new(py);
            c.arg("-m").arg("pytest");
            c
        }
        None => Command::new("pytest"),
    };
    cmd.current_dir(base_dir);
    cmd.arg(format!("--cov={}", cov_target.display()));
    cmd.arg("--cov-context=test");
    if append {
        cmd.arg("--cov-append");
    }
    for arg in extra {
        cmd.arg(arg);
    }
    for t in targets {
        // Pass targets relative to base_dir so recorded pytest node ids stay
        // relative — matches the purge prefixes and the reader's rebasing.
        cmd.arg(rel_to(t, base_dir).unwrap_or_else(|| t.clone()));
    }
    let status = cmd
        .status()
        .context("spawning pytest (is pytest + pytest-cov installed?)")?;
    // pytest exits non-zero when tests fail. Coverage data is still written,
    // so warn rather than abort — the user wants the .coverage either way.
    if !status.success() {
        eprintln!(
            "warning: pytest exited with a failure status; coverage was still \
             recorded for the tests that ran."
        );
    }
    Ok(())
}

/// Test files under `tests` modified strictly after `since`.
fn changed_test_files(tests: &Path, since: SystemTime) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !tests.exists() {
        return out;
    }
    for entry in walkdir::WalkDir::new(tests)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("py") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified > since {
                    out.push(p.to_path_buf());
                }
            }
        }
    }
    out.sort();
    out
}

/// Delete every context whose pytest node id belongs to one of `prefixes`
/// (a test-file path like `tests/test_a.py`), plus its `line_bits`/`arc`
/// rows. Returns how many context rows were removed. Matches both the bare
/// path and any `path::...` node id.
fn purge_contexts(db_path: &Path, prefixes: &[String]) -> Result<usize> {
    use rusqlite::Connection;
    if prefixes.is_empty() {
        return Ok(0);
    }
    let conn = Connection::open(db_path).context("opening coverage database")?;
    let mut removed = 0usize;
    for prefix in prefixes {
        let like = format!("{prefix}::%");
        // Collect matching context ids first.
        let ids: Vec<i64> = {
            let mut stmt =
                conn.prepare("SELECT id FROM context WHERE context = ?1 OR context LIKE ?2")?;
            let rows = stmt.query_map((prefix, &like), |r| r.get::<_, i64>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for id in ids {
            conn.execute("DELETE FROM line_bits WHERE context_id = ?1", [id])?;
            // `arc` only exists when branch coverage was recorded; ignore the
            // "no such table" error so line-only databases still purge.
            let _ = conn.execute("DELETE FROM arc WHERE context_id = ?1", [id]);
            removed += conn.execute("DELETE FROM context WHERE id = ?1", [id])?;
        }
    }
    Ok(removed)
}

/// Print a short "what now" footer. If the loaded config doesn't already wire
/// coverage, tell the user the one thing they need to do.
fn report_done(loaded: &LoadedConfig, db_path: &Path, appended: Option<usize>) {
    match appended {
        Some(n) => println!("\nAppended coverage for {n} changed file(s)."),
        None => println!("\nWrote {}.", display_rel(db_path, &loaded.base_dir)),
    }
    let configured = loaded.file.coverage.is_some();
    if configured {
        println!("`fermut run` will use it.");
    } else {
        let rel = display_rel(db_path, &loaded.base_dir);
        println!(
            "To use it, run `fermut run --coverage {rel}` \
             (or add `coverage = \"{rel}\"` to your fermut config so it's automatic)."
        );
    }
}

/// `path` relative to `base`, or `None` when it isn't a descendant.
fn rel_to(path: &Path, base: &Path) -> Option<PathBuf> {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let base_abs = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    abs.strip_prefix(&base_abs).ok().map(|p| p.to_path_buf())
}

/// Display `path` relative to `base` when possible, else as given.
fn display_rel(path: &Path, base: &Path) -> String {
    rel_to(path, base)
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn make_db(dir: &Path) -> PathBuf {
        let db = dir.join(".coverage");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE context (id integer primary key, context text, unique(context));
             CREATE TABLE line_bits (file_id integer, context_id integer, numbits blob);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO context (id, context) VALUES
                (1, 'tests/test_a.py::test_x|run'),
                (2, 'tests/test_a.py::test_y|run'),
                (3, 'tests/test_b.py::test_z|run')",
            (),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO line_bits (file_id, context_id, numbits) VALUES (1,1,x'02'),(1,2,x'04'),(1,3,x'08')",
            (),
        )
        .unwrap();
        db
    }

    #[test]
    fn purge_removes_only_matching_file_contexts() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_db(tmp.path());

        let removed = purge_contexts(&db, &["tests/test_a.py".to_string()]).unwrap();
        assert_eq!(removed, 2); // test_x and test_y

        let conn = Connection::open(&db).unwrap();
        let remaining: Vec<String> = conn
            .prepare("SELECT context FROM context ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(remaining, vec!["tests/test_b.py::test_z|run".to_string()]);

        // line_bits for purged contexts gone; test_b's bits remain.
        let bits: i64 = conn
            .query_row("SELECT count(*) FROM line_bits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bits, 1);
    }

    #[test]
    fn purge_is_noop_for_unknown_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_db(tmp.path());
        let removed = purge_contexts(&db, &["tests/nope.py".to_string()]).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn changed_test_files_respects_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let tests = tmp.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        let old = tests.join("test_old.py");
        std::fs::write(&old, "def test_a(): pass\n").unwrap();

        // Everything written before this instant is "old".
        let cutoff = SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let new = tests.join("test_new.py");
        std::fs::write(&new, "def test_b(): pass\n").unwrap();

        let changed = changed_test_files(&tests, cutoff);
        assert_eq!(changed, vec![new]);
    }
}
