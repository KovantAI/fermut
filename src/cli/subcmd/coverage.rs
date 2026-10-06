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
//!
//! A **deleted source file** is dropped from the DB (`prune_orphan_files`) and
//! the tests that covered it re-run, so a moved/renamed module is measured at
//! its new path without a `--full` rebuild. The prune checks the filesystem and
//! refuses to act when most recorded files look missing (a DB restored onto a
//! different checkout root), so it can't wipe a cached CI database.

use std::collections::BTreeMap;
use std::io::IsTerminal;
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

    // Absolute: pytest runs from `base_dir`, not our cwd, and gets this path
    // via `COVERAGE_FILE`, so a relative `--output` would resolve differently
    // for the two of us.
    let db_path = opts
        .output
        .clone()
        .map(|p| std::path::absolute(&p).unwrap_or(p))
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
            &db_path,
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

    let Some(plan) = plan_incremental(&current, &prior, &db_path, &base_dir)? else {
        println!(
            "{} is up to date (no test or source content changed since it was written). \
             Nothing to do.",
            display_rel(&db_path, &base_dir)
        );
        return Ok(());
    };
    let IncrementalPlan {
        changed_sources,
        deleted_tests,
        deleted_sources,
        tests_for_sources,
        reimported,
        rerun,
    } = plan;

    let touched_sources = changed_sources.len() + deleted_sources.len();
    if touched_sources > 0 {
        // A changed source that nothing in the DB covers re-runnably (covered
        // only by an untracked test — a root conftest, a test outside the tests
        // dir — or its coverers were deleted). We can't refresh that coverage
        // incrementally; the baseline still advances so the change isn't
        // re-detected forever. A pure deletion with no coverer is fine — its
        // rows are pruned below, nothing goes stale.
        let uncovered = changed_sources
            .iter()
            .filter(|s| !reimported.contains(*s))
            .count();
        if uncovered > 0 {
            eprintln!(
                "warning: {uncovered} source file(s) changed but no re-runnable test covers \
                 them in {} — their coverage may be stale. Run `fermut coverage --full` \
                 to rebuild.",
                display_rel(&db_path, &base_dir)
            );
        }
        if !tests_for_sources.is_empty() {
            println!(
                "{touched_sources} changed or removed source file(s); re-measuring the {} \
                 test file(s) that cover them.",
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

    // Import-time lines (module bodies, `def`/`class` headers) are recorded
    // under the shared empty `""` context, which no test-file purge reaches,
    // and `--cov-append` unions them. For a changed file that leaves its
    // pre-change line numbers marked executed forever. Drop that file's `""`
    // bits so the re-run rebuilds them — only for files the re-run will
    // re-import (changed tests, and changed sources with a re-run coverer);
    // purging anything else would lose its import-time coverage until `--full`.
    if !rerun.is_empty() {
        purge_import_time(&db_path, &reimported).with_context(|| {
            format!(
                "purging stale import-time coverage from {}",
                db_path.display()
            )
        })?;
    }

    // Drop `file` rows for sources that no longer exist. Runs after the coverer
    // query above (which reads those rows) and checks the filesystem rather
    // than the fingerprint diff, so deletions from before this existed heal too.
    let pruned_files = match prune_orphan_files(&db_path, &base_dir, &source)
        .with_context(|| format!("pruning removed source files from {}", db_path.display()))?
    {
        Prune::Pruned(paths) => {
            if !paths.is_empty() {
                println!("dropping {} removed source file(s):", paths.len());
                if paths.len() <= 10 || std::io::stdout().is_terminal() {
                    for p in &paths {
                        println!("  {p}");
                    }
                }
            }
            paths.len()
        }
        Prune::Skipped { missing, total } => {
            eprintln!(
                "warning: {missing} of {total} source file(s) recorded in {} are missing on \
                 disk — likely a coverage DB restored onto a different checkout root, not \
                 real deletions. Not pruning; run `fermut coverage --full` to rebuild.",
                display_rel(&db_path, &base_dir)
            );
            0
        }
    };

    let ran_ok = if rerun.is_empty() {
        true // deletions-only refresh: nothing to run, purge already applied
    } else {
        run_pytest(
            &base_dir,
            &cov_target,
            &db_path,
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
            pruned_files,
        },
    );
    Ok(())
}

/// What an incremental refresh has to do, derived from the fingerprint diff and
/// the DB's recorded coverers. `None` from [`plan_incremental`] means nothing
/// changed.
#[derive(Debug)]
struct IncrementalPlan {
    /// Sources whose content is new or modified.
    changed_sources: Vec<String>,
    /// Test files in the baseline that no longer exist — their contexts are purged.
    deleted_tests: Vec<String>,
    /// Source files in the baseline that no longer exist — their `file` rows are
    /// pruned and their coverers re-run.
    deleted_sources: Vec<String>,
    /// Existing test files whose recorded contexts cover a changed or deleted
    /// source.
    tests_for_sources: Vec<PathBuf>,
    /// Changed files (base-dir-relative keys) the re-run is sure to import
    /// again: every changed test, plus each changed source with at least one
    /// existing coverer in `rerun`. Their stale import-time bits get purged.
    reimported: Vec<String>,
    /// Deduplicated, existing test files to re-run under `--cov-append`.
    rerun: Vec<PathBuf>,
}

/// Diff `current` against `prior` and work out which tests to re-measure.
///
/// Content diffs: changed tests re-run because the test changed; changed
/// sources matter because any test covering them now has a stale line mapping;
/// deleted tests must have their contexts purged so a removed test stops
/// marking lines covered; deleted sources must leave the DB, and the tests that
/// covered them re-run — after a move they most likely exercise the module's
/// new path, which otherwise would have no coverage until `--full`.
///
/// Must run before [`prune_orphan_files`]: the coverer query reads the very
/// `file` rows the prune deletes.
fn plan_incremental(
    current: &Fingerprints,
    prior: &Fingerprints,
    db_path: &Path,
    base_dir: &Path,
) -> Result<Option<IncrementalPlan>> {
    let changed_tests = changed_paths(&current.tests, &prior.tests, base_dir);
    let changed_sources = changed_keys(&current.sources, &prior.sources);
    let deleted_tests = deleted_keys(&current.tests, &prior.tests);
    let deleted_sources = deleted_keys(&current.sources, &prior.sources);

    // Genuinely nothing changed → done. Gate on the raw diffs, not on `rerun`:
    // a changed source with no re-runnable coverer leaves `rerun` empty but is
    // NOT "up to date" — the caller still advances the baseline so we don't
    // re-detect it every run. Same for a deletion-only change, which still has
    // rows to prune.
    if changed_tests.is_empty()
        && changed_sources.is_empty()
        && deleted_tests.is_empty()
        && deleted_sources.is_empty()
    {
        return Ok(None);
    }

    // Tests whose recorded contexts cover a changed or deleted source file —
    // re-run so their line mapping is rebuilt against the new source tree. This
    // is the source-awareness that makes the incremental DB safe for `--since`.
    //
    // Filtered to paths that still exist: the DB may list a test deleted in this
    // same change, and handing a missing path to pytest aborts collection (its
    // stale contexts are purged via `deleted_tests`). Filtering here keeps the
    // re-measure count accurate for the message too.
    //
    // Changed sources are queried one at a time so we know which of them the
    // re-run is guaranteed to import (see `IncrementalPlan::reimported`).
    let coverers = |srcs: &[String]| -> Result<Vec<PathBuf>> {
        if srcs.is_empty() {
            return Ok(Vec::new());
        }
        Ok(tests_covering_sources(db_path, srcs)
            .with_context(|| format!("querying {} for source coverage", db_path.display()))?
            .into_iter()
            .map(|rel| base_dir.join(rel))
            .filter(|f| f.exists())
            .collect())
    };
    let mut tests_for_sources = std::collections::BTreeSet::<PathBuf>::new();
    let mut reimported: Vec<String> = changed_keys(&current.tests, &prior.tests);
    for src in &changed_sources {
        let found = coverers(std::slice::from_ref(src))?;
        if !found.is_empty() {
            reimported.push(src.clone());
            tests_for_sources.extend(found);
        }
    }
    tests_for_sources.extend(coverers(&deleted_sources)?);
    let tests_for_sources: Vec<PathBuf> = tests_for_sources.into_iter().collect();

    // Union the two re-run sets by base-dir-relative key, skipping any path that
    // no longer exists on disk (see above).
    let mut rerun: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for f in changed_tests.iter().chain(tests_for_sources.iter()) {
        if !f.exists() {
            continue;
        }
        if let Some(key) =
            rel_to(f, base_dir).and_then(|p| p.to_str().map(|s| s.replace('\\', "/")))
        {
            if seen.insert(key) {
                rerun.push(f.clone());
            }
        }
    }

    Ok(Some(IncrementalPlan {
        changed_sources,
        deleted_tests,
        deleted_sources,
        tests_for_sources,
        reimported,
        rerun,
    }))
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

/// Keys present in `prior` but gone from `current` (deleted files).
fn deleted_keys(
    current: &BTreeMap<String, String>,
    prior: &BTreeMap<String, String>,
) -> Vec<String> {
    prior
        .keys()
        .filter(|k| !current.contains_key(*k))
        .cloned()
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
        let suffix = like_suffix(src);
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT c.context \
             FROM line_bits lb \
             JOIN context c ON c.id = lb.context_id \
             WHERE lb.file_id IN ({FILE_IDS_MATCHING})"
        ))?;
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

/// `file` ids whose stored path is `?1` exactly or ends in `/?1` (`?2` is
/// [`like_suffix`] of it). coverage.py stores paths absolute or relative
/// depending on `relative_files`, hence the suffix arm.
const FILE_IDS_MATCHING: &str = "SELECT id FROM file WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'";

/// LIKE pattern matching any stored path ending in `/<rel>`. Escapes LIKE
/// metacharacters so a filename's `_` (ubiquitous in Python) or `%` matches
/// literally, not as a wildcard — otherwise `my_module.py` would also match
/// `myXmodule.py`. `\` is the ESCAPE char in [`FILE_IDS_MATCHING`].
fn like_suffix(rel: &str) -> String {
    let escaped = rel
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%/{escaped}")
}

/// Delete the import-time coverage (`line_bits` / `arc` rows under the empty
/// `""` context) of each file in `file_rels`, leaving every other file's
/// import-time bits and all per-test contexts alone. Returns the number of
/// `line_bits` rows removed.
fn purge_import_time(db_path: &Path, file_rels: &[String]) -> Result<usize> {
    use rusqlite::Connection;
    if file_rels.is_empty() {
        return Ok(0);
    }
    let mut conn = Connection::open(db_path).context("opening coverage database")?;
    let tx = conn.transaction()?;
    let mut removed = 0usize;
    for rel in file_rels {
        let suffix = like_suffix(rel);
        let filter = format!(
            "context_id IN (SELECT id FROM context WHERE context = '') \
             AND file_id IN ({FILE_IDS_MATCHING})"
        );
        removed += tx.execute(
            &format!("DELETE FROM line_bits WHERE {filter}"),
            (rel, &suffix),
        )?;
        // `arc` only exists when branch coverage was recorded.
        let _ = tx.execute(&format!("DELETE FROM arc WHERE {filter}"), (rel, &suffix));
    }
    tx.commit()?;
    Ok(removed)
}

/// Invoke `pytest <targets> --cov=<target> --cov-context=test [--cov-append]`
/// from `base_dir`, inheriting stdio so the user sees the live test run.
/// Coverage data goes to `db_path` (via `COVERAGE_FILE`), the same file every
/// purge, prune and sidecar step here operates on. Returns whether pytest
/// exited 0, so the caller can decide whether to advance the fingerprint
/// baseline.
fn run_pytest(
    base_dir: &Path,
    cov_target: &Path,
    db_path: &Path,
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
    // Without this pytest-cov writes `<base_dir>/.coverage` (or the project's
    // `[run] data_file`) whatever `--output` says, so the data and the file we
    // purge, append to and fingerprint drift apart. coverage.py lets the env
    // var override the config file's `data_file`.
    cmd.env("COVERAGE_FILE", db_path);
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
    let mut conn = Connection::open(db_path).context("opening coverage database")?;
    // One transaction: a crash mid-loop must not leave a half-purged DB whose
    // sidecar still claims the old baseline.
    let tx = conn.transaction()?;
    let conn = &tx;
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
    tx.commit()?;
    Ok(removed)
}

/// Outcome of [`prune_orphan_files`].
#[derive(Debug, PartialEq, Eq)]
enum Prune {
    /// These base-dir-relative paths (absolute when outside `base_dir`) were
    /// dropped from the DB. Empty when nothing was orphaned.
    Pruned(Vec<String>),
    /// Too many recorded files are missing for it to be real deletions — most
    /// likely the DB was restored onto a different checkout root. Nothing was
    /// touched.
    Skipped { missing: usize, total: usize },
}

/// Delete `file` rows (and their `line_bits` / `arc` / `tracer` children)
/// whose path resolves under `source_root` but no longer exists on disk.
///
/// Trusts the filesystem rather than the fingerprint diff, so a DB that
/// already carries stale rows (e.g. a cached CI generation from before this
/// prune existed) heals on its next incremental run.
///
/// Conservative by construction, since a wrong resolution would wipe the DB:
/// - relative paths resolve against `base_dir` (pytest's cwd); absolute paths
///   count only when under `source_root` in raw or canonical form (macOS
///   `/var` vs `/private/var`). Anything else — a foreign CI root, a `..`
///   path, a file outside the measured tree — is kept;
/// - if every file under the root, or more than half of them, is missing, it
///   prunes nothing and returns [`Prune::Skipped`]. Pure deletions that large
///   are rare; a checkout-root mismatch is the likely explanation.
fn prune_orphan_files(db_path: &Path, base_dir: &Path, source_root: &Path) -> Result<Prune> {
    use rusqlite::Connection;
    let mut conn = Connection::open(db_path).context("opening coverage database")?;

    let forms = |p: &Path| -> Vec<PathBuf> {
        let mut v = vec![p.to_path_buf()];
        if let Ok(c) = p.canonicalize() {
            if c != p {
                v.push(c);
            }
        }
        v
    };
    let base_forms = forms(base_dir);
    let mut source_forms = forms(source_root);
    // `source_root` as seen from each base form, so a canonical base pairs with
    // a canonical source even when only the base is a symlink.
    if let Some(rel) = rel_to(source_root, base_dir) {
        for b in &base_forms {
            let s = b.join(&rel);
            if !source_forms.contains(&s) {
                source_forms.push(s);
            }
        }
    }

    // (file id, display path) for every row under the source root, and the
    // subset missing on disk.
    let mut total = 0usize;
    let mut missing: Vec<(i64, String)> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id, path FROM file")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, stored) = row?;
            let stored_path = Path::new(&stored);
            let abs = if stored_path.is_absolute() {
                stored_path.to_path_buf()
            } else if stored_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                continue; // can't place it confidently → keep
            } else {
                base_dir.join(stored_path)
            };
            if !source_forms.iter().any(|s| abs.starts_with(s)) {
                continue; // outside the measured tree (or a foreign root) → keep
            }
            total += 1;
            if !abs.exists() {
                let shown = base_forms
                    .iter()
                    .find_map(|b| abs.strip_prefix(b).ok())
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|| abs.display().to_string());
                missing.push((id, shown));
            }
        }
    }

    if missing.is_empty() {
        return Ok(Prune::Pruned(Vec::new()));
    }
    if missing.len() == total || missing.len() * 2 > total {
        return Ok(Prune::Skipped {
            missing: missing.len(),
            total,
        });
    }

    let tx = conn.transaction()?;
    for (id, _) in &missing {
        tx.execute("DELETE FROM line_bits WHERE file_id = ?1", [id])?;
        // `arc` exists only with branch coverage and `tracer` only when a
        // plugin/tracer was recorded; ignore "no such table" on line-only DBs.
        let _ = tx.execute("DELETE FROM arc WHERE file_id = ?1", [id]);
        let _ = tx.execute("DELETE FROM tracer WHERE file_id = ?1", [id]);
        tx.execute("DELETE FROM file WHERE id = ?1", [id])?;
    }
    // Drop contexts the prune left with no data. Harmless to keep, but they'd
    // otherwise accumulate as the tree churns.
    let has_arc: bool = tx.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'arc'",
        [],
        |r| r.get(0),
    )?;
    let orphan_ctx = if has_arc {
        "DELETE FROM context WHERE id NOT IN (SELECT context_id FROM line_bits) \
         AND id NOT IN (SELECT context_id FROM arc)"
    } else {
        "DELETE FROM context WHERE id NOT IN (SELECT context_id FROM line_bits)"
    };
    tx.execute(orphan_ctx, [])?;
    tx.commit()?;

    Ok(Prune::Pruned(missing.into_iter().map(|(_, p)| p).collect()))
}

/// What a `fermut coverage` invocation did, for the summary line.
enum Done {
    /// A full-suite (re)build wrote the whole database.
    Full,
    /// An incremental refresh re-ran `reran` test files, dropped `removed`
    /// deleted test files' contexts and `pruned_files` deleted source files
    /// from the database.
    Incremental {
        reran: usize,
        removed: usize,
        pruned_files: usize,
    },
}

/// Print a short "what now" footer. If the loaded config doesn't already wire
/// coverage, tell the user the one thing they need to do.
fn report_done(loaded: &LoadedConfig, db_path: &Path, done: Done) {
    match done {
        Done::Full => println!("\nWrote {}.", display_rel(db_path, &loaded.base_dir)),
        Done::Incremental {
            reran,
            removed,
            pruned_files,
        } => {
            let mut parts: Vec<String> = Vec::new();
            if reran > 0 {
                parts.push(format!("re-ran {reran} test file(s)"));
            }
            if removed > 0 {
                parts.push(format!("dropped {removed} removed test file(s)"));
            }
            if pruned_files > 0 {
                parts.push(format!("dropped {pruned_files} removed source file(s)"));
            }
            if parts.is_empty() {
                // Source changed but no coverer was re-runnable (see the warning
                // above); the baseline advanced but the DB is otherwise unchanged.
                println!("\nCoverage baseline updated (nothing to re-run).");
            } else {
                println!("\nRefreshed coverage — {}.", parts.join(", "));
            }
        }
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
        make_db_with_files_opts(dir, false)
    }

    /// [`make_db_with_files`], optionally with the branch-coverage `arc` table
    /// and the `tracer` table populated for both files.
    fn make_db_with_files_opts(dir: &Path, arcs_and_tracer: bool) -> PathBuf {
        let db = dir.join(".coverage");
        let conn = Connection::open(&db).unwrap();
        if arcs_and_tracer {
            conn.execute_batch(
                "CREATE TABLE arc (file_id integer, context_id integer, fromno integer, tono integer);
                 CREATE TABLE tracer (file_id integer primary key, tracer text);
                 INSERT INTO arc VALUES (10,1,1,2),(10,3,1,2),(11,2,1,2);
                 INSERT INTO tracer VALUES (10,''),(11,'');",
            )
            .unwrap();
        }
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

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Lay out `src/foo.py` (and `src/bar.py` unless `bar_deleted`) on disk to
    /// match [`make_db_with_files`].
    fn write_sources(base: &Path, bar_deleted: bool) {
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/foo.py"), "x = 1\n").unwrap();
        if !bar_deleted {
            std::fs::write(base.join("src/bar.py"), "y = 1\n").unwrap();
        }
    }

    /// Add `n` more live, covered files so a single deletion stays under the
    /// prune safety valve's majority threshold.
    fn add_live_files(db: &Path, base: &Path, n: i64) {
        let conn = Connection::open(db).unwrap();
        for i in 0..n {
            let rel = format!("src/live{i}.py");
            std::fs::write(base.join(&rel), "z = 1\n").unwrap();
            conn.execute(
                "INSERT INTO file (id, path) VALUES (?1, ?2)",
                (100 + i, &rel),
            )
            .unwrap();
            conn.execute("INSERT INTO line_bits VALUES (?1, 1, x'02')", [100 + i])
                .unwrap();
        }
    }

    #[test]
    fn prune_removes_orphan_file_and_children() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files_opts(base, true);
        write_sources(base, true); // src/bar.py deleted
        add_live_files(&db, base, 1);

        let got = prune_orphan_files(&db, base, &base.join("src")).unwrap();
        assert_eq!(got, Prune::Pruned(vec!["src/bar.py".to_string()]));

        let conn = Connection::open(&db).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM file WHERE id = 11"), 0);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM line_bits WHERE file_id = 11"),
            0
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM arc WHERE file_id = 11"),
            0
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM tracer WHERE file_id = 11"),
            0
        );
        // Live foo.py untouched.
        assert_eq!(count(&conn, "SELECT count(*) FROM file WHERE id = 10"), 1);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM line_bits WHERE file_id = 10"),
            2
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM arc WHERE file_id = 10"),
            2
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM tracer WHERE file_id = 10"),
            1
        );
        // test_bar's context only had bar.py data → dropped; the others stay.
        assert_eq!(count(&conn, "SELECT count(*) FROM context WHERE id = 2"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM context"), 2);
    }

    #[test]
    fn prune_tolerates_line_only_db() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files(base); // no arc / tracer tables
        write_sources(base, true);
        add_live_files(&db, base, 1);

        let got = prune_orphan_files(&db, base, &base.join("src")).unwrap();
        assert_eq!(got, Prune::Pruned(vec!["src/bar.py".to_string()]));
        let conn = Connection::open(&db).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM file"), 2);
    }

    #[test]
    fn prune_is_noop_when_nothing_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files(base);
        write_sources(base, false);

        let got = prune_orphan_files(&db, base, &base.join("src")).unwrap();
        assert_eq!(got, Prune::Pruned(Vec::new()));
        let conn = Connection::open(&db).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM file"), 2);
        assert_eq!(count(&conn, "SELECT count(*) FROM context"), 3);
    }

    #[test]
    fn prune_keeps_paths_outside_source_root() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files(base);
        write_sources(base, false);
        // Missing, but outside the measured `src/` tree (and a `..` escape).
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO file (id, path) VALUES (20, 'other/gone.py'), (21, '../gone.py')",
            (),
        )
        .unwrap();

        let got = prune_orphan_files(&db, base, &base.join("src")).unwrap();
        assert_eq!(got, Prune::Pruned(Vec::new()));
        assert_eq!(count(&conn, "SELECT count(*) FROM file"), 4);
    }

    #[test]
    fn prune_skips_when_all_paths_missing() {
        // Simulates a CI cache restored onto a different checkout: every
        // recorded path under the root is "missing". Must not wipe the DB.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files_opts(base, true);
        std::fs::create_dir_all(base.join("src")).unwrap(); // no files on disk

        let got = prune_orphan_files(&db, base, &base.join("src")).unwrap();
        assert_eq!(
            got,
            Prune::Skipped {
                missing: 2,
                total: 2
            }
        );
        let conn = Connection::open(&db).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM file"), 2);
        assert_eq!(count(&conn, "SELECT count(*) FROM line_bits"), 3);
        assert_eq!(count(&conn, "SELECT count(*) FROM context"), 3);
    }

    #[test]
    fn prune_skips_when_majority_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files(base);
        std::fs::create_dir_all(base.join("src")).unwrap();
        add_live_files(&db, base, 1); // 1 live of 3 → 2/3 missing

        let got = prune_orphan_files(&db, base, &base.join("src")).unwrap();
        assert_eq!(
            got,
            Prune::Skipped {
                missing: 2,
                total: 3
            }
        );
    }

    #[test]
    fn prune_resolves_absolute_stored_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files(base);
        write_sources(base, false);
        add_live_files(&db, base, 2);
        // coverage.py records realpath'd absolute keys (`/private/var/...` on
        // macOS), so store the canonical form of a missing file under base.
        let canon_base = base.canonicalize().unwrap();
        let gone = canon_base.join("src/gone.py");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO file (id, path) VALUES (30, ?1), (31, '/home/runner/work/proj/src/foo.py')",
            [gone.to_str().unwrap()],
        )
        .unwrap();

        let got = prune_orphan_files(&db, base, &base.join("src")).unwrap();
        assert_eq!(got, Prune::Pruned(vec!["src/gone.py".to_string()]));
        // The foreign-root path is kept: it can't be placed under our root.
        assert_eq!(count(&conn, "SELECT count(*) FROM file WHERE id = 31"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM file WHERE id = 30"), 0);
    }

    fn fp(tests: &[&str], sources: &[&str]) -> Fingerprints {
        let m = |ks: &[&str]| {
            ks.iter()
                .map(|k| (k.to_string(), "h".to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        Fingerprints {
            v: CURRENT_FP_V,
            tests: m(tests),
            sources: m(sources),
        }
    }

    #[test]
    fn unchanged_tree_is_up_to_date() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_db_with_files(tmp.path());
        let same = fp(&["tests/test_foo.py"], &["src/foo.py"]);
        assert!(plan_incremental(&same, &same, &db, tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleted_source_only_is_not_up_to_date() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_db_with_files(tmp.path());
        let prior = fp(&[], &["src/foo.py", "src/bar.py"]);
        let current = fp(&[], &["src/foo.py"]);
        let plan = plan_incremental(&current, &prior, &db, tmp.path())
            .unwrap()
            .expect("a deletion-only change must not short-circuit as up to date");
        assert_eq!(plan.deleted_sources, vec!["src/bar.py".to_string()]);
        assert!(plan.changed_sources.is_empty());
    }

    #[test]
    fn deleted_source_reruns_its_coverers() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files(base);
        std::fs::create_dir_all(base.join("tests")).unwrap();
        for t in ["test_foo.py", "test_bar.py", "test_both.py"] {
            std::fs::write(base.join("tests").join(t), "def test_t(): pass\n").unwrap();
        }
        let tests = [
            "tests/test_foo.py",
            "tests/test_bar.py",
            "tests/test_both.py",
        ];
        let prior = fp(&tests, &["src/foo.py", "src/bar.py"]);
        let current = fp(&tests, &["src/foo.py"]);

        let plan = plan_incremental(&current, &prior, &db, base)
            .unwrap()
            .unwrap();
        let rerun: Vec<String> = plan
            .rerun
            .iter()
            .filter_map(|f| rel_to(f, base))
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        // bar.py's coverer only — foo.py is unchanged.
        assert_eq!(rerun, vec!["tests/test_bar.py".to_string()]);
    }

    #[test]
    fn purge_import_time_drops_only_target_files_empty_context() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_db_with_files_opts(tmp.path(), true);
        let conn = Connection::open(&db).unwrap();
        // Import-time bits for both files under the shared `""` context.
        conn.execute_batch(
            "INSERT INTO context (id, context) VALUES (4, '');
             INSERT INTO line_bits VALUES (10,4,x'ff'),(11,4,x'ff');
             INSERT INTO arc VALUES (10,4,-1,1),(11,4,-1,1);",
        )
        .unwrap();

        let removed = purge_import_time(&db, &["src/foo.py".to_string()]).unwrap();
        assert_eq!(removed, 1);
        // foo's import-time bits gone; bar's kept.
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM line_bits WHERE context_id = 4 AND file_id = 10"
            ),
            0
        );
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM arc WHERE context_id = 4 AND file_id = 10"
            ),
            0
        );
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM line_bits WHERE context_id = 4 AND file_id = 11"
            ),
            1
        );
        // Per-test bits for foo untouched; the shared `""` context row survives.
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM line_bits WHERE context_id IN (1,3)"
            ),
            2
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM context WHERE id = 4"), 1);
    }

    #[test]
    fn purge_import_time_tolerates_line_only_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_db_with_files(tmp.path()); // no arc table, no `""` context
        assert_eq!(
            purge_import_time(&db, &["src/foo.py".to_string()]).unwrap(),
            0
        );
    }

    #[test]
    fn reimported_covers_changed_tests_and_rerun_sources_only() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let db = make_db_with_files(base);
        std::fs::create_dir_all(base.join("tests")).unwrap();
        for t in ["test_foo.py", "test_bar.py", "test_both.py", "test_new.py"] {
            std::fs::write(base.join("tests").join(t), "def test_t(): pass\n").unwrap();
        }
        let prior = fp(
            &[
                "tests/test_foo.py",
                "tests/test_bar.py",
                "tests/test_both.py",
            ],
            &["src/foo.py", "src/orphan.py"],
        );
        let mut current = fp(
            &[
                "tests/test_foo.py",
                "tests/test_bar.py",
                "tests/test_both.py",
                "tests/test_new.py",
            ],
            &["src/foo.py", "src/orphan.py"],
        );
        // foo.py has coverers in the DB; orphan.py has none.
        current.sources.insert("src/foo.py".into(), "h2".into());
        current.sources.insert("src/orphan.py".into(), "h2".into());

        let plan = plan_incremental(&current, &prior, &db, base)
            .unwrap()
            .unwrap();
        let mut got = plan.reimported.clone();
        got.sort();
        // The new test re-runs (so re-imports); foo.py's coverers re-run; nothing
        // re-imports orphan.py, so its import-time bits must be left alone.
        assert_eq!(
            got,
            vec!["src/foo.py".to_string(), "tests/test_new.py".to_string()]
        );
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
