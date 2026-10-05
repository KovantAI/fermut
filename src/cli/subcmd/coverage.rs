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
//! 3. If one exists, re-runs only the tests a change actually invalidated and
//!    appends them, so a refresh costs a handful of test files, not the suite.
//!
//! fermut reads the `.coverage` SQLite directly (see
//! [`crate::filter::coverage`]), so there is no `coverage json` export step.
//!
//! ## What "changed" means (content, and source-aware)
//!
//! Change detection is by **content hash**, recorded in a `.coverage`-adjacent
//! `*.fermut-fingerprints.json` sidecar — deliberately NOT mtime, which every
//! CI checkout rewrites (an mtime check would re-measure everything each run
//! and collapse the incremental path into a full rebuild).
//!
//! Two kinds of change invalidate recorded coverage, and both are handled:
//!   1. a **test file** changed → re-run that test;
//!   2. a **source file** changed → every test whose contexts cover it now has
//!      a stale line→test mapping (added/shifted lines), so re-run those tests
//!      too. This second half is what makes the incremental DB safe for a
//!      diff-scoped `fermut run --since`, whose mutants sit on exactly those
//!      changed source lines — without it, `--since` would read stale coverage
//!      and mis-scope or mis-select tests for the changed lines.
//!
//! Residual gap: a source change that introduces a *new* code path covered by
//! an *unchanged* test is not caught (the DB only knows pre-change coverage), so
//! that path re-measures on the next full/test-triggered refresh. A mutant there
//! is conservatively coverage-skipped meanwhile, never falsely killed.
//!
//! ## Incremental correctness
//!
//! `pytest --cov-append` *unions* coverage data per context. For a brand-new
//! test that is exactly right. For a *modified* test that now executes fewer
//! lines, the old line bits would linger and falsely mark a line as covered.
//! So before appending we DELETE the re-run and removed files' contexts from the
//! DB (`purge_contexts`), guaranteeing their line bits are rebuilt from scratch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::loader::LoadedConfig;

/// Current fingerprint-sidecar schema version. `load_fingerprints` rejects any
/// sidecar not stamped with exactly this value, so bumping it here genuinely
/// forces a full rebuild after a format change (an older/newer sidecar is
/// discarded rather than deserialized into incompatible semantics).
const CURRENT_FP_V: u32 = 1;

/// Content fingerprints of the test AND source trees, stored beside the
/// `.coverage` DB so the incremental refresh can tell what actually changed by
/// **content** — not mtime (useless in CI, where every checkout rewrites it) —
/// and re-measure exactly the tests whose recorded coverage a change
/// invalidated.
///
/// Two kinds of change invalidate a test's recorded coverage:
///
/// 1. the **test** itself changed — re-run that test file;
/// 2. a **source** file it covers changed — the recorded line→test mapping is
///    now stale (added/shifted lines), so re-run every test whose contexts
///    touched that source file. Tracking source hashes is what makes the
///    incremental DB safe for a diff-scoped `fermut run` (`--since`), whose
///    mutants sit on exactly those changed source lines.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Fingerprints {
    /// Schema version. Always written as [`CURRENT_FP_V`]; a sidecar carrying
    /// any other value is discarded on load, triggering a full rebuild.
    v: u32,
    /// base-dir-relative test-file path (forward slashes) → hex sha256.
    #[serde(default)]
    tests: BTreeMap<String, String>,
    /// base-dir-relative source-file path (forward slashes) → hex sha256.
    #[serde(default)]
    sources: BTreeMap<String, String>,
}

pub(crate) fn coverage(opts: CoverageArgs) -> Result<()> {
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

    let current = current_fingerprints(&tests, &source, &base_dir);
    let prior = load_fingerprints(&db_path);

    // Full rebuild — forced, no DB yet, or a DB with no fermut sidecar (an old
    // fermut, or a `.coverage` from a bare pytest-cov run). Without the sidecar
    // we can't tell what changed, so rebuilding is the only sound option and it
    // seeds the baseline every later run is incremental against.
    let full = opts.full || !db_path.exists() || prior.is_none();
    if full {
        if db_path.exists() && prior.is_none() && !opts.full {
            println!(
                "{} has no fermut fingerprint sidecar; rebuilding fully to establish an \
                 incremental baseline.",
                display_rel(&db_path, &base_dir)
            );
        } else {
            println!("Running full test suite under coverage…");
        }
        let ran_ok = run_pytest(
            &base_dir,
            &cov_target,
            std::slice::from_ref(&tests),
            false,
            &opts.pytest_args,
            python.as_deref(),
        )?;
        // Only record the baseline if the suite actually passed. A failed full
        // run wrote partial coverage; saving the sidecar would mark it complete
        // and make the next `fermut coverage` skip as "up to date". Leaving no
        // sidecar keeps the run on the full-rebuild path until it goes green.
        if ran_ok {
            save_fingerprints(&db_path, &current)?;
        } else {
            eprintln!(
                "warning: not writing the coverage fingerprint baseline because pytest \
                 failed — the next `fermut coverage` will rebuild from scratch."
            );
        }
        report_done(&loaded, &db_path, Done::Full);
        return Ok(());
    }
    let prior = prior.unwrap_or_default();

    // Content diffs. `changed_tests` re-run because the test changed;
    // `changed_sources` matter because any test covering them now has a stale
    // line mapping; `deleted_tests` must have their contexts purged so a removed
    // test stops marking lines covered.
    let changed_tests = changed_paths(&current.tests, &prior.tests, &base_dir);
    let changed_sources = changed_keys(&current.sources, &prior.sources);
    let deleted_tests: Vec<String> = prior
        .tests
        .keys()
        .filter(|k| !current.tests.contains_key(*k))
        .cloned()
        .collect();

    // Tests whose recorded contexts cover a changed source file — re-run so
    // their line mapping is rebuilt against the new source. This is the
    // source-awareness that makes the incremental DB safe for `--since`.
    //
    // Filtered to paths that still exist: the DB may list a test deleted in this
    // same change, and handing a missing path to pytest aborts collection (its
    // stale contexts are purged below via `deleted_tests`). Filtering here keeps
    // the re-measure count accurate for the message too.
    let tests_for_sources: Vec<PathBuf> = if changed_sources.is_empty() {
        Vec::new()
    } else {
        tests_covering_sources(&db_path, &changed_sources)
            .with_context(|| format!("querying {} for source coverage", db_path.display()))?
            .into_iter()
            .map(|rel| base_dir.join(rel))
            .filter(|f| f.exists())
            .collect()
    };

    // Union the two re-run sets by base-dir-relative key. Skip any path that no
    // longer exists on disk: `tests_covering_sources` reads the DB, which still
    // lists a test deleted in this same change — handing that missing path to
    // pytest is a usage error that aborts collection and appends NOTHING for the
    // other (valid) files too. Its stale contexts are still purged below via
    // `deleted_tests`.
    let mut rerun: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for f in changed_tests.iter().chain(tests_for_sources.iter()) {
        if !f.exists() {
            continue;
        }
        if let Some(key) =
            rel_to(f, &base_dir).and_then(|p| p.to_str().map(|s| s.replace('\\', "/")))
        {
            if seen.insert(key) {
                rerun.push(f.clone());
            }
        }
    }

    // Genuinely nothing changed → done. Gate on the raw diffs, not on `rerun`:
    // a changed source with no re-runnable coverer leaves `rerun` empty but is
    // NOT "up to date" — we still advance the baseline below so we don't
    // re-detect it every run.
    if changed_tests.is_empty() && changed_sources.is_empty() && deleted_tests.is_empty() {
        println!(
            "{} is up to date (no test or source content changed since it was written). \
             Nothing to do.",
            display_rel(&db_path, &base_dir)
        );
        return Ok(());
    }

    if !changed_sources.is_empty() {
        if tests_for_sources.is_empty() {
            // Source changed but nothing in the DB covers it that we can re-run
            // (covered only by an untracked test — a root conftest, a test
            // outside the tests dir — or its coverers were deleted). We can't
            // refresh that coverage incrementally; the baseline still advances
            // so the change isn't re-detected forever.
            eprintln!(
                "warning: {} source file(s) changed but no re-runnable test covers them in \
                 {} — their coverage may be stale. Run `fermut coverage --full` to rebuild.",
                changed_sources.len(),
                display_rel(&db_path, &base_dir)
            );
        } else {
            println!(
                "{} changed source file(s); re-measuring the {} test file(s) that cover them.",
                changed_sources.len(),
                tests_for_sources.len()
            );
        }
    }
    if !rerun.is_empty() {
        println!("appending coverage for {} test file(s):", rerun.len());
        for f in &rerun {
            println!("  {}", display_rel(f, &base_dir));
        }
    }
    if !deleted_tests.is_empty() {
        println!(
            "purging contexts for {} removed test file(s).",
            deleted_tests.len()
        );
    }

    // Purge stale contexts for every file we're about to re-run (a modified test
    // covering fewer lines must not keep old bits) plus removed tests.
    let mut prefixes: Vec<String> = rerun
        .iter()
        .filter_map(|f| rel_to(f, &base_dir))
        .filter_map(|p| p.to_str().map(|s| s.replace('\\', "/")))
        .collect();
    prefixes.extend(deleted_tests.iter().cloned());
    let purged = purge_contexts(&db_path, &prefixes)
        .with_context(|| format!("purging stale contexts from {}", db_path.display()))?;
    if purged > 0 {
        println!("Purged {purged} stale context(s).");
    }

    let ran_ok = if rerun.is_empty() {
        true // deletions-only refresh: nothing to run, purge already applied
    } else {
        run_pytest(
            &base_dir,
            &cov_target,
            &rerun,
            true,
            &opts.pytest_args,
            python.as_deref(),
        )?
    };

    // Advance the baseline only on success. A failed pytest already purged the
    // re-run files' contexts but did not re-add them; leaving the old sidecar in
    // place means the next `fermut coverage` re-detects the same changes and
    // retries, rather than treating the missing coverage as fresh.
    if ran_ok {
        save_fingerprints(&db_path, &current)?;
    } else {
        eprintln!(
            "warning: not advancing the coverage fingerprint baseline because pytest \
             failed — the next `fermut coverage` will retry these files."
        );
    }
    report_done(
        &loaded,
        &db_path,
        Done::Incremental {
            reran: rerun.len(),
            removed: deleted_tests.len(),
        },
    );
    Ok(())
}

/// Sidecar path: the DB path with `.fermut-fingerprints.json` appended, so a
/// cache that captures the `.coverage` DB captures its baseline too.
fn fingerprints_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".fermut-fingerprints.json");
    PathBuf::from(s)
}

fn load_fingerprints(db_path: &Path) -> Option<Fingerprints> {
    let raw = std::fs::read_to_string(fingerprints_path(db_path)).ok()?;
    let fp: Fingerprints = serde_json::from_str(&raw).ok()?;
    // A different schema version means the stored maps may not mean what this
    // binary expects — discard it (→ full rebuild) rather than act on it.
    if fp.v != CURRENT_FP_V {
        return None;
    }
    Some(fp)
}

fn save_fingerprints(db_path: &Path, fp: &Fingerprints) -> Result<()> {
    let path = fingerprints_path(db_path);
    let raw = serde_json::to_string_pretty(fp).context("serializing coverage fingerprints")?;
    std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Content fingerprints of both trees, keyed by base-dir-relative path with
/// forward slashes (matching purge prefixes and recorded pytest node ids).
fn current_fingerprints(tests: &Path, source: &Path, base_dir: &Path) -> Fingerprints {
    let tests_fp = fingerprint_tree(tests, base_dir);
    let mut sources = fingerprint_tree(source, base_dir);
    // When the tests tree lives under the source tree (e.g. source_root ".",
    // tests "tests"), a test file is walked by both. It is a test — keep it only
    // in `tests`, so it isn't double-counted as a "changed source" and doesn't
    // fire a spurious source-coverage query.
    sources.retain(|k, _| !tests_fp.contains_key(k));
    Fingerprints {
        v: CURRENT_FP_V,
        tests: tests_fp,
        sources,
    }
}

/// Hash every `.py` file under `dir`, keyed by base-dir-relative forward-slash
/// path. Uses the gitignore-aware walker (same as the engine's test-suite
/// fingerprint): a `source_root` of `.` must not drag in `.venv`/site-packages
/// or `__pycache__` — those are gitignored/pruned, never part of the run.
fn fingerprint_tree(dir: &Path, base_dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if !dir.exists() {
        return out;
    }
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false) // .gitignore handles `.venv`; keep non-ignored dotfiles
        .git_ignore(true)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let name = entry.file_name().to_str();
            !(is_dir && name.is_some_and(|n| n.starts_with('.') || n == "__pycache__"))
        })
        .build();
    for entry in walker.flatten() {
        let p = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("py") {
            continue;
        }
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let Some(rel) = rel_to(p, base_dir) else {
            continue;
        };
        let key = rel.to_string_lossy().replace('\\', "/");
        let mut h = Sha256::new();
        h.update(&bytes);
        out.insert(key, hex::encode(h.finalize()));
    }
    out
}

/// Keys whose value differs between `current` and `prior` (new or modified).
fn changed_keys(
    current: &BTreeMap<String, String>,
    prior: &BTreeMap<String, String>,
) -> Vec<String> {
    current
        .iter()
        .filter(|(k, v)| prior.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Like [`changed_keys`] but resolves each changed key back to an absolute path
/// under `base_dir` (for handing to pytest).
fn changed_paths(
    current: &BTreeMap<String, String>,
    prior: &BTreeMap<String, String>,
    base_dir: &Path,
) -> Vec<PathBuf> {
    changed_keys(current, prior)
        .into_iter()
        .map(|k| base_dir.join(k))
        .collect()
}

/// Distinct test files whose recorded contexts cover any of `source_rels`
/// (base-dir-relative, forward-slash source paths), returned as base-dir-relative
/// paths. This is the DB half of source-aware invalidation: a changed source
/// file's mutants need fresh line→test mapping, which only re-running its
/// covering tests produces.
///
/// coverage.py stores source paths absolute or relative depending on
/// `relative_files`, so each is matched by exact or path-suffix equality. A
/// context node id is `<file>::<test>|<phase>`; the file part is the test file
/// to re-run.
fn tests_covering_sources(db_path: &Path, source_rels: &[String]) -> Result<Vec<PathBuf>> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening coverage database")?;
    let mut test_files = std::collections::BTreeSet::<String>::new();
    for src in source_rels {
        // Escape LIKE metacharacters so a filename's `_` (ubiquitous in Python)
        // or `%` matches literally, not as a wildcard — otherwise `my_module.py`
        // would also match `myXmodule.py`. The exact `path = ?1` arm needs no
        // escaping. `\` is the ESCAPE char below.
        let escaped = src
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let suffix = format!("%/{escaped}");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT c.context \
             FROM line_bits lb \
             JOIN context c ON c.id = lb.context_id \
             WHERE lb.file_id IN \
                 (SELECT id FROM file WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\')",
        )?;
        let rows = stmt.query_map((src, &suffix), |r| r.get::<_, String>(0))?;
        for ctx in rows.filter_map(|r| r.ok()) {
            // `<file>::<test>|<phase>` → `<file>`; also handles a context with
            // no `::` (module-level) by stripping the trailing `|<phase>`.
            let file = ctx
                .split("::")
                .next()
                .unwrap_or(&ctx)
                .split('|')
                .next()
                .unwrap_or("")
                .trim_start_matches("./");
            if !file.is_empty() {
                test_files.insert(file.to_string());
            }
        }
    }
    Ok(test_files.into_iter().map(PathBuf::from).collect())
}

/// Invoke `pytest <targets> --cov=<target> --cov-context=test [--cov-append]`
/// from `base_dir`, inheriting stdio so the user sees the live test run.
/// Returns whether pytest exited 0, so the caller can decide whether to advance
/// the fingerprint baseline.
fn run_pytest(
    base_dir: &Path,
    cov_target: &Path,
    targets: &[PathBuf],
    append: bool,
    extra: &[String],
    python: Option<&Path>,
) -> Result<bool> {
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
    crate::runner::sanitize_python_env(&mut cmd);
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
    // pytest exits non-zero when tests fail. Coverage data is still written for
    // the tests that ran, so warn rather than abort — the user wants the
    // `.coverage` either way. Returns whether it succeeded so the caller can
    // decide whether to advance the fingerprint baseline (a failed run must not
    // mark files as freshly measured).
    if !status.success() {
        eprintln!(
            "warning: pytest exited with a failure status; coverage was still \
             recorded for the tests that ran."
        );
    }
    Ok(status.success())
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

/// What a `fermut coverage` invocation did, for the summary line.
enum Done {
    /// A full-suite (re)build wrote the whole database.
    Full,
    /// An incremental refresh re-ran `reran` test files and dropped `removed`
    /// deleted ones from the database.
    Incremental { reran: usize, removed: usize },
}

/// Print a short "what now" footer. If the loaded config doesn't already wire
/// coverage, tell the user the one thing they need to do.
fn report_done(loaded: &LoadedConfig, db_path: &Path, done: Done) {
    match done {
        Done::Full => println!("\nWrote {}.", display_rel(db_path, &loaded.base_dir)),
        Done::Incremental { reran, removed } => match (reran, removed) {
            // Source changed but no coverer was re-runnable (see the warning
            // above); the baseline advanced but the DB is otherwise unchanged.
            (0, 0) => println!("\nCoverage baseline updated (nothing to re-run)."),
            (0, r) => println!("\nRefreshed coverage — dropped {r} removed test file(s)."),
            (n, 0) => println!("\nRefreshed coverage — re-ran {n} test file(s)."),
            (n, r) => {
                println!("\nRefreshed coverage — re-ran {n} test file(s), dropped {r} removed.")
            }
        },
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
    fn fingerprint_tree_tracks_content_not_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let tests = base.join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(tests.join("test_a.py"), "def test_a(): pass\n").unwrap();

        let a = fingerprint_tree(&tests, base);
        assert!(a.contains_key("tests/test_a.py"));

        // Rewrite identical bytes, newer mtime → hash must not move (the CI case
        // where checkout rewrites every mtime).
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(tests.join("test_a.py"), "def test_a(): pass\n").unwrap();
        assert_eq!(
            a,
            fingerprint_tree(&tests, base),
            "content-stable across mtime"
        );

        // Real content change moves it.
        std::fs::write(tests.join("test_a.py"), "def test_a(): assert True\n").unwrap();
        let c = fingerprint_tree(&tests, base);
        assert_ne!(a["tests/test_a.py"], c["tests/test_a.py"]);
    }

    #[test]
    fn fingerprint_tree_prunes_gitignored_and_pycache() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src = base.join("src");
        std::fs::create_dir_all(src.join("__pycache__")).unwrap();
        std::fs::create_dir_all(src.join("vendored")).unwrap();
        std::fs::write(src.join("foo.py"), "x = 1\n").unwrap();
        // gitignored subtree (stands in for `.venv`/site-packages).
        std::fs::write(src.join(".gitignore"), "vendored/\n").unwrap();
        std::fs::write(src.join("vendored").join("huge.py"), "y = 2\n").unwrap();
        // stray bytecode + a .py under __pycache__ — never part of the run.
        std::fs::write(src.join("__pycache__").join("foo.cpython.py"), "z = 3\n").unwrap();

        let fp = fingerprint_tree(&src, base);
        assert_eq!(
            fp.keys().cloned().collect::<Vec<_>>(),
            vec!["src/foo.py".to_string()],
            "only the tracked source file, not gitignored/__pycache__ trees"
        );
    }

    #[test]
    fn source_fingerprint_excludes_tests_living_under_source() {
        // source_root ".", tests nested under it — a test file must count once,
        // as a test, not also as a "changed source".
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src = base.join("src");
        let tests = src.join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(src.join("foo.py"), "x = 1\n").unwrap();
        std::fs::write(tests.join("test_x.py"), "def test_x(): pass\n").unwrap();

        let fp = current_fingerprints(&tests, &src, base);
        assert!(fp.tests.contains_key("src/tests/test_x.py"));
        assert!(fp.sources.contains_key("src/foo.py"));
        assert!(
            !fp.sources.contains_key("src/tests/test_x.py"),
            "a test under the source tree must not double-count as source"
        );
    }

    #[test]
    fn changed_keys_detects_new_and_modified() {
        let prior = BTreeMap::from([
            ("a.py".to_string(), "h1".to_string()),
            ("b.py".to_string(), "h2".to_string()),
        ]);
        let current = BTreeMap::from([
            ("a.py".to_string(), "h1".to_string()),  // unchanged
            ("b.py".to_string(), "h2b".to_string()), // modified
            ("c.py".to_string(), "h3".to_string()),  // new
        ]);
        let mut changed = changed_keys(&current, &prior);
        changed.sort();
        assert_eq!(changed, vec!["b.py".to_string(), "c.py".to_string()]);
    }

    #[test]
    fn load_rejects_sidecar_with_mismatched_version() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".coverage");
        // A sidecar from a future schema version must be discarded (→ full
        // rebuild), not deserialized into today's semantics.
        std::fs::write(
            fingerprints_path(&db),
            format!(
                r#"{{"v":{},"tests":{{}},"sources":{{}}}}"#,
                CURRENT_FP_V + 1
            ),
        )
        .unwrap();
        assert!(load_fingerprints(&db).is_none());

        // The current version loads fine.
        std::fs::write(
            fingerprints_path(&db),
            format!(r#"{{"v":{CURRENT_FP_V},"tests":{{}},"sources":{{}}}}"#),
        )
        .unwrap();
        assert!(load_fingerprints(&db).is_some());
    }

    #[test]
    fn fingerprints_roundtrip_through_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".coverage");
        assert!(load_fingerprints(&db).is_none());
        let fp = Fingerprints {
            v: 1,
            tests: BTreeMap::from([("tests/t.py".to_string(), "h".to_string())]),
            sources: BTreeMap::from([("src/s.py".to_string(), "h2".to_string())]),
        };
        save_fingerprints(&db, &fp).unwrap();
        assert!(fingerprints_path(&db).exists());
        let loaded = load_fingerprints(&db).unwrap();
        assert_eq!(loaded.tests, fp.tests);
        assert_eq!(loaded.sources, fp.sources);
    }

    /// A coverage DB where `src/foo.py` is covered by test_foo + test_both, and
    /// `src/bar.py` by test_bar. Used to prove source-aware invalidation picks
    /// exactly the tests that cover a changed source file.
    fn make_db_with_files(dir: &Path) -> PathBuf {
        let db = dir.join(".coverage");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE context (id integer primary key, context text, unique(context));
             CREATE TABLE file (id integer primary key, path text, unique(path));
             CREATE TABLE line_bits (file_id integer, context_id integer, numbits blob);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO context (id, context) VALUES
                (1, 'tests/test_foo.py::t|run'),
                (2, 'tests/test_bar.py::t|run'),
                (3, 'tests/test_both.py::t|run')",
            (),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file (id, path) VALUES (10, 'src/foo.py'), (11, 'src/bar.py')",
            (),
        )
        .unwrap();
        // foo covered by test_foo(1) + test_both(3); bar covered by test_bar(2).
        conn.execute(
            "INSERT INTO line_bits (file_id, context_id, numbits) VALUES
                (10,1,x'02'),(10,3,x'02'),(11,2,x'02')",
            (),
        )
        .unwrap();
        db
    }

    #[test]
    fn tests_covering_sources_selects_only_covering_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_db_with_files(tmp.path());

        let mut got: Vec<String> = tests_covering_sources(&db, &["src/foo.py".to_string()])
            .unwrap()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        got.sort();
        // foo's coverers, not test_bar.
        assert_eq!(
            got,
            vec![
                "tests/test_foo.py".to_string(),
                "tests/test_both.py".to_string()
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );

        // A source nothing covers → no tests.
        let none = tests_covering_sources(&db, &["src/unknown.py".to_string()]).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn tests_covering_sources_treats_underscore_literally() {
        // `_` is a LIKE wildcard; a source `my_module.py` must not match a
        // sibling `myXmodule.py` and drag in its unrelated test.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".coverage");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE context (id integer primary key, context text, unique(context));
             CREATE TABLE file (id integer primary key, path text, unique(path));
             CREATE TABLE line_bits (file_id integer, context_id integer, numbits blob);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO context (id, context) VALUES
                (1, 'tests/test_mine.py::t|run'),
                (2, 'tests/test_other.py::t|run')",
            (),
        )
        .unwrap();
        // Two absolute source paths differing only at the `_` position.
        conn.execute(
            "INSERT INTO file (id, path) VALUES
                (10, '/abs/proj/src/my_module.py'),
                (11, '/abs/proj/src/myXmodule.py')",
            (),
        )
        .unwrap();
        conn.execute("INSERT INTO line_bits VALUES (10,1,x'02'),(11,2,x'02')", ())
            .unwrap();

        let got = tests_covering_sources(&db, &["src/my_module.py".to_string()]).unwrap();
        assert_eq!(
            got,
            vec![PathBuf::from("tests/test_mine.py")],
            "underscore must match literally, not myXmodule.py's test"
        );
    }

    #[test]
    fn tests_covering_sources_matches_absolute_stored_paths() {
        // coverage.py stores absolute source paths when relative_files is off;
        // the suffix match must still find them.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".coverage");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE context (id integer primary key, context text, unique(context));
             CREATE TABLE file (id integer primary key, path text, unique(path));
             CREATE TABLE line_bits (file_id integer, context_id integer, numbits blob);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO context (id, context) VALUES (1, 'tests/test_x.py::t|run')",
            (),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file (id, path) VALUES (10, '/home/runner/work/proj/src/foo.py')",
            (),
        )
        .unwrap();
        conn.execute("INSERT INTO line_bits VALUES (10,1,x'02')", ())
            .unwrap();

        let got = tests_covering_sources(&db, &["src/foo.py".to_string()]).unwrap();
        assert_eq!(got, vec![PathBuf::from("tests/test_x.py")]);
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct CoverageArgs {
    /// Where to start project-root discovery. Defaults to cwd.
    #[arg(default_value = ".")]
    pub(crate) path: std::path::PathBuf,

    /// Measured source root (`--cov=<this>`). Defaults to the configured
    /// `source_root`, else the project root.
    #[arg(long)]
    pub(crate) source: Option<std::path::PathBuf>,

    /// Tests location. Defaults to the configured `tests`, else `<source>/tests`.
    #[arg(long)]
    pub(crate) tests: Option<std::path::PathBuf>,

    /// Re-run the full suite even when an up-to-date `.coverage` exists.
    #[arg(long)]
    pub(crate) full: bool,

    /// Database output path. Defaults to `<project>/.coverage`.
    #[arg(long)]
    pub(crate) output: Option<std::path::PathBuf>,

    /// Python interpreter (path) or virtualenv (dir) to run pytest with,
    /// same as `fermut run --python`. Lets coverage generation work
    /// without `pytest` on PATH. Auto-discovers a venv when omitted.
    #[arg(long, value_name = "PATH")]
    pub(crate) python: Option<std::path::PathBuf>,

    /// Extra arguments forwarded to pytest. Repeatable.
    #[arg(long = "pytest-arg", value_name = "ARG")]
    pub(crate) pytest_args: Vec<String>,
}
