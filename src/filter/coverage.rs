//! Coverage-driven test selection (plus the "filter" side effect of skipping
//! mutants on lines no test ever executed).
//!
//! Reads a `coverage.json` produced by coverage.py and indexes the per-line
//! **test contexts** — the list of pytest node ids that executed each line.
//! Generated with:
//!
//! ```sh
//! pytest --cov=src --cov-context=test    # requires pytest-cov; emits pytest nodeIDs
//! coverage json -o coverage.json --show-contexts
//! fermut run path --coverage coverage.json
//! ```
//!
//! `coverage run --context=LABEL` sets a single static label, not per-test
//! contexts, and won't work here. Coverage.py's `dynamic_context = test_function`
//! does produce per-test contexts but as dotted Python module paths, which
//! are not valid pytest selectors when fed back via `-k`. pytest-cov is the
//! only path that yields selectors fermut can hand back to pytest.
//!
//! What fermut does with the result:
//!
//! 1. **Filter:** mutants on lines with no test context are skipped (running
//!    them would always survive, since no test exercises the line).
//! 2. **Select:** for the lines that do have contexts, only those tests are
//!    passed to pytest per mutant. Cuts wall time dramatically when each
//!    mutant only needs a handful of tests instead of the full suite.
//!
//! Both consult a mutant through [`CoverageContexts::tests_for_mutant`], which
//! falls back to the head line of the mutant's enclosing statement. Some
//! continuation lines never appear in the contexts map even though the
//! statement around them ran: CPython folds a collection literal of three or
//! more constant elements into a single constant load attributed to the
//! literal's first line, so the element lines emit no line event and can carry
//! no test context. Without the fallback every mutant on such a line is
//! dropped as "uncovered" and no test can rescue it.
//!
//! This is narrower than "coverage.py is statement-granular". That holds for
//! the report's `executed_lines`, which collapses continuation lines onto the
//! statement head — but not for the per-line `contexts` map fermut reads, where
//! a wrapped call's argument lines, a boolean operand on its own line and a
//! literal below the folding threshold do each get their own entry. See
//! `docs/guides/coverage.md`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, warn};

use super::Filter;
use crate::mutator::Mutant;

/// Per-file, per-line index of test node ids that touched the line.
#[derive(Debug, Default)]
pub struct CoverageContexts {
    map: HashMap<PathBuf, HashMap<u32, Vec<String>>>,
    /// Cache mapping caller-supplied paths to the canonical form used in `map`.
    /// Populated lazily on first miss so per-mutant lookups skip a `canonicalize`
    /// syscall after the first hit per unique source file.
    canonical_cache: RwLock<HashMap<PathBuf, PathBuf>>,
    /// Per-test coverage breadth: how many `(file, line)` cells each test node id
    /// executes across the whole coverage map. Computed once from `map`. Smart
    /// ordering uses it as a cold-start prior — a test touching fewer cells is a
    /// narrower test, so a likelier focused killer, and runs first when no kill
    /// history exists yet. Breadth is global (not scoped to the mutated file), so
    /// it's a coarse specificity signal: it can misrank a broad-but-relevant
    /// killer, which only weakens the `-x` short-circuit — never the verdict.
    test_breadth: HashMap<String, u32>,
}

/// Intermediate shape both readers (JSON export, SQLite DB) produce before
/// the shared `from_records` rebases and indexes them: per file, the list of
/// `(line, raw test contexts)` pairs.
type CoverageRecords = Vec<(String, Vec<(u32, Vec<String>)>)>;

#[derive(Deserialize)]
struct CoverageDoc {
    files: HashMap<String, FileCoverage>,
}

#[derive(Deserialize, Default)]
struct FileCoverage {
    #[serde(default)]
    contexts: HashMap<String, Vec<String>>,
}

impl CoverageContexts {
    /// Read a coverage file, auto-detecting format. coverage.py's native
    /// `.coverage` is a SQLite database; the `coverage json` export is JSON.
    /// We sniff the SQLite magic header (`SQLite format 3\0`) rather than
    /// trust the extension, since users name the DB anything (`.coverage`,
    /// `.coverage.ci`, no extension at all).
    ///
    /// Prefer the SQLite path: it stores each pytest nodeid once (vs the JSON
    /// export repeating it per covered line), so a multi-GB `coverage.json`
    /// is typically tens of MB as SQLite and parses without the
    /// read-whole-file-into-a-String peak.
    pub fn from_path(path: &Path, source_root: &Path, project_root: &Path) -> Result<Arc<Self>> {
        // A missing coverage file is the #1 first-run faceplant: `fermut init`
        // wires `coverage = "coverage.json"` when pytest-cov is detected, but
        // the file doesn't exist until the user generates it. Catch it here
        // with the exact two commands to run instead of leaking a bare
        // `No such file or directory (os error 2)` from the open below.
        if !path.exists() {
            anyhow::bail!(
                "coverage file `{}` not found.\n\n\
                 fermut uses per-test coverage to pick which tests to run for each \
                 mutant. Generate it first (needs pytest-cov):\n  \
                 pytest --cov={src} --cov-context=test\n  \
                 coverage json -o {cov} --show-contexts\n\n\
                 Or run without coverage selection: pass `--no-coverage`, or remove \
                 the `coverage` key from fermut.toml.",
                path.display(),
                src = source_root.display(),
                cov = path.display(),
            );
        }
        if is_sqlite(path)? {
            Self::from_coverage_db(path, source_root, project_root)
        } else {
            Self::from_json(path, source_root, project_root)
        }
    }

    /// Read coverage.py's native `.coverage` SQLite database directly.
    ///
    /// Schema (coverage.py v7): `file(id, path)`, `context(id, context)`,
    /// `line_bits(file_id, context_id, numbits)` where `numbits` is a bitmap
    /// blob — byte `b` bit `i` set means line `b*8 + i` executed. We invert
    /// the per-(file,context) bitmaps into the per-(file,line) test index the
    /// filter wants.
    pub fn from_coverage_db(
        path: &Path,
        source_root: &Path,
        project_root: &Path,
    ) -> Result<Arc<Self>> {
        use rusqlite::Connection;

        let conn = Connection::open(path)
            .with_context(|| format!("opening coverage database {}", path.display()))?;

        // context id -> nodeid string (with the `|run|setup|teardown` phase
        // suffix still attached; clean_test_ids strips it later).
        let mut ctx_by_id: HashMap<i64, String> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, context FROM context")
                .context("preparing context query")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .context("querying context table")?;
            for row in rows {
                let (id, ctx) = row.context("reading context row")?;
                ctx_by_id.insert(id, ctx);
            }
        }

        // file id -> path string (coverage.py stores these absolute).
        let mut path_by_file: HashMap<i64, String> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, path FROM file")
                .context("preparing file query")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .context("querying file table")?;
            for row in rows {
                let (id, p) = row.context("reading file row")?;
                path_by_file.insert(id, p);
            }
        }

        // Invert line_bits into file_key -> line -> raw nodeids.
        let mut per_file: HashMap<String, HashMap<u32, Vec<String>>> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT file_id, context_id, numbits FROM line_bits")
                .context("preparing line_bits query")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .context("querying line_bits table")?;
            for row in rows {
                let (fid, cid, blob) = row.context("reading line_bits row")?;
                let Some(file_key) = path_by_file.get(&fid) else {
                    continue;
                };
                let Some(nodeid) = ctx_by_id.get(&cid) else {
                    continue;
                };
                let lines = per_file.entry(file_key.clone()).or_default();
                for line in numbits_to_lines(&blob) {
                    lines.entry(line).or_default().push(nodeid.clone());
                }
            }
        }

        let records: CoverageRecords = per_file
            .into_iter()
            .map(|(file, lines)| (file, lines.into_iter().collect()))
            .collect();
        Self::from_records(records, path, source_root, project_root)
    }

    /// Parse a `coverage.json` with contexts.
    pub fn from_json(path: &Path, source_root: &Path, project_root: &Path) -> Result<Arc<Self>> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading coverage file {}", path.display()))?;
        let doc: CoverageDoc = serde_json::from_str(&raw)
            .with_context(|| format!("parsing coverage JSON {}", path.display()))?;

        let records: CoverageRecords = doc
            .files
            .into_iter()
            .map(|(file, info)| {
                let lines = info
                    .contexts
                    .into_iter()
                    .filter_map(|(line_str, tests)| {
                        line_str.parse::<u32>().ok().map(|line| (line, tests))
                    })
                    .collect();
                (file, lines)
            })
            .collect();
        Self::from_records(records, path, source_root, project_root)
    }

    /// Shared core: take per-file, per-line raw test contexts (from either the
    /// JSON export or the SQLite DB) and build the lookup index.
    ///
    /// `source_root` is the directory being mutated; `project_root` is the
    /// anchor pytest will use as cwd in the mirror (typically the nearest
    /// `pyproject.toml` ancestor of `source_root`). The coverage cwd — where
    /// `coverage run` was invoked — is auto-detected by probing for the
    /// recorded file keys under `source_root` and its ancestors. Recorded
    /// pytest node ids are then rebased so they resolve relative to
    /// `project_root`, regardless of whether coverage was run from
    /// `project_root`, a monorepo parent, or a subdirectory.
    fn from_records(
        records: CoverageRecords,
        path: &Path,
        source_root: &Path,
        project_root: &Path,
    ) -> Result<Arc<Self>> {
        let file_keys: Vec<&str> = records.iter().map(|(f, _)| f.as_str()).collect();
        let coverage_cwd = detect_coverage_cwd(source_root, &file_keys);
        // Canonicalize both anchors so prefix stripping works under symlink
        // roots (e.g., `/tmp` → `/private/tmp` on macOS). Without this,
        // rebase_node_id strips against `/private/var/...` while abs paths
        // carry `/var/...` and every id falls outside project_root.
        let coverage_cwd = coverage_cwd.canonicalize().unwrap_or(coverage_cwd);
        let canonical_project = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());

        let mut map: HashMap<PathBuf, HashMap<u32, Vec<String>>> = HashMap::new();
        let mut any_contexts = false;
        let mut rebase_cache: HashMap<String, Option<String>> = HashMap::new();
        let mut dropped_outside = 0usize;

        for (file, lines) in records {
            let p = coverage_cwd.join(&file);
            let p = p.canonicalize().unwrap_or(p);
            let mut per_line: HashMap<u32, Vec<String>> = HashMap::new();
            for (line, raw_tests) in lines {
                let cleaned = clean_test_ids(&raw_tests);
                let mut rebased: BTreeSet<String> = BTreeSet::new();
                for id in cleaned {
                    match rebase_node_id(&id, &coverage_cwd, &canonical_project, &mut rebase_cache)
                    {
                        Some(new_id) => {
                            rebased.insert(new_id);
                        }
                        None => dropped_outside += 1,
                    }
                }
                if !rebased.is_empty() {
                    any_contexts = true;
                    per_line.insert(line, rebased.into_iter().collect());
                }
            }
            if !per_line.is_empty() {
                map.insert(p, per_line);
            }
        }

        if dropped_outside > 0 {
            warn!(
                count = dropped_outside,
                project_root = %project_root.display(),
                "dropped test contexts whose path falls outside project_root; \
                 pytest cannot resolve them from the mirror"
            );
        }

        if !any_contexts {
            if dropped_outside > 0 {
                return Err(anyhow::anyhow!(
                    "coverage.json at {} has per-test contexts, but all {} fall \
                     outside project_root {}. Tests appear to live in a sibling \
                     package; point `--project-root` at the directory that \
                     contains them (or the monorepo root) so pytest can resolve \
                     the rebased node ids.",
                    path.display(),
                    dropped_outside,
                    project_root.display(),
                ));
            }
            return Err(anyhow::anyhow!(
                "coverage.json at {} has no per-test contexts. Regenerate with \
                 `pytest --cov=src --cov-context=test` (requires pytest-cov) \
                 followed by `coverage json -o coverage.json --show-contexts`. \
                 Note: `coverage run --context=LABEL` sets only a static label \
                 and won't produce per-test selectors.",
                path.display()
            ));
        }

        // Per-test breadth: count the `(file, line)` cells each node id covers.
        // One pass over the built map — cheap, done once at load.
        let mut test_breadth: HashMap<String, u32> = HashMap::new();
        for per_line in map.values() {
            for ids in per_line.values() {
                for id in ids {
                    *test_breadth.entry(id.clone()).or_insert(0) += 1;
                }
            }
        }

        Ok(Arc::new(Self {
            map,
            canonical_cache: RwLock::new(HashMap::new()),
            test_breadth,
        }))
    }

    /// Per-test coverage breadth (number of covered `(file, line)` cells),
    /// keyed by test node id. Empty when there are no contexts. The smart-order
    /// cold-start prior reads this to prefer more-targeted tests.
    pub fn breadth_map(&self) -> &HashMap<String, u32> {
        &self.test_breadth
    }

    /// Reorder coverage-selected test node ids so the narrowest test — the one
    /// covering the fewest `(file, line)` cells globally — runs first. Under
    /// pytest's `-x` a focused unit test is the likelier killer, so trying it
    /// first lets the mutant die (and the run return) sooner. This is the
    /// cold-start ordering prior: it needs no run history, only the coverage
    /// already loaded, so it helps on run 1 and on freshly-changed `--since`
    /// lines.
    ///
    /// Borrows the input — the returned refs point back into `ids`; the caller
    /// keeps `ids` alive. `sort_by_key` is stable, so ties (and ids absent from
    /// the breadth map, treated as maximally broad → sorted last) keep the
    /// caller's order. **Only permutes** the set — never adds or drops an id —
    /// so the kill/survive verdict is unchanged; only which test pytest tries
    /// first.
    pub fn order_by_breadth<'a>(&self, ids: &'a [String]) -> Vec<&'a String> {
        let mut ordered: Vec<&String> = ids.iter().collect();
        ordered.sort_by_key(|id| self.test_breadth.get(*id).copied().unwrap_or(u32::MAX));
        ordered
    }

    /// Test node ids that executed `file:line`, or `None` if none recorded.
    /// Caller distinguishes "no contexts → skip mutant" from "selection empty".
    ///
    /// Caller paths often already match the canonical form stored in `map`
    /// (mutants and coverage typically share an absolute prefix), so we try a
    /// direct lookup first. On miss we canonicalize and cache the mapping —
    /// unique-file count is small relative to mutant count, so cache hit rate
    /// is high after warm-up.
    pub fn tests_for(&self, file: &Path, line: u32) -> Option<&[String]> {
        if let Some(per_line) = self.map.get(file) {
            return per_line.get(&line).map(|v| v.as_slice());
        }
        if let Some(canonical) = self
            .canonical_cache
            .read()
            .ok()
            .and_then(|g| g.get(file).cloned())
        {
            return self
                .map
                .get(&canonical)
                .and_then(|per_line| per_line.get(&line))
                .map(|v| v.as_slice());
        }
        let canonical = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        if let Ok(mut guard) = self.canonical_cache.write() {
            guard.insert(file.to_path_buf(), canonical.clone());
        }
        self.map
            .get(&canonical)
            .and_then(|per_line| per_line.get(&line))
            .map(|v| v.as_slice())
    }

    /// Test node ids that executed a mutant, or `None` if none recorded.
    ///
    /// Tries the mutated line first, then the head line of the enclosing
    /// statement — the line coverage.py attributes the whole statement's
    /// execution to. Both the filter and per-mutant test selection go through
    /// here so they cannot disagree about whether a mutant is covered.
    pub fn tests_for_mutant(&self, m: &Mutant) -> Option<&[String]> {
        if let Some(tests) = self.tests_for(&m.file, m.line) {
            if !tests.is_empty() {
                return Some(tests);
            }
        }
        if m.stmt_line == 0 || m.stmt_line == m.line {
            return None;
        }
        self.tests_for(&m.file, m.stmt_line)
    }
}

/// Coverage.py tags every context with a phase suffix: `|run`, `|setup`,
/// `|teardown`. Strip it so we get a stable pytest nodeid back. Empty contexts
/// (the `""` covering import-time execution) are dropped — they don't map to
/// a test we can run.
fn clean_test_ids(raw: &[String]) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for id in raw {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            continue;
        }
        let node = trimmed
            .rsplit_once('|')
            .map(|(left, _)| left)
            .unwrap_or(trimmed)
            .to_string();
        if !node.is_empty() {
            seen.insert(node);
        }
    }
    seen.into_iter().collect()
}

/// Decode a coverage.py `numbits` bitmap blob into line numbers. Byte `b`
/// bit `i` (LSB-first) set means line number `b * 8 + i` was executed. This
/// is the inverse of coverage.py's `nums_to_numbits`.
fn numbits_to_lines(blob: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    for (b, byte) in blob.iter().enumerate() {
        for i in 0..8u32 {
            if byte & (1 << i) != 0 {
                out.push(b as u32 * 8 + i);
            }
        }
    }
    out
}

/// Sniff whether a file is a SQLite database by its 16-byte magic header
/// (`SQLite format 3\0`). coverage.py's `.coverage` is SQLite; the
/// `coverage json` export is JSON. We check the header rather than the
/// extension because the DB has no canonical name.
fn is_sqlite(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening coverage file {}", path.display()))?;
    let mut header = [0u8; 16];
    match f.read(&mut header) {
        Ok(n) => Ok(n >= 16 && &header == b"SQLite format 3\0"),
        Err(e) => Err(e).with_context(|| format!("reading coverage file {}", path.display())),
    }
}

/// Find the directory `coverage run` was invoked from by probing for the
/// recorded file keys. Coverage.py records paths relative to that cwd, so
/// the lowest ancestor of `source_root` where the keys resolve on disk is
/// our answer. Falls back to `source_root` if nothing matches — same shape
/// as the old behavior, which assumed coverage and source share a cwd.
fn detect_coverage_cwd(source_root: &Path, keys: &[&str]) -> PathBuf {
    if keys.is_empty() {
        return source_root.to_path_buf();
    }
    // 8 is enough to disambiguate any realistic project layout while staying
    // cheap. A real mismatch shows up on the first key; we sample a few more
    // only so a single oddly-named file doesn't dominate the decision.
    let sample: Vec<&str> = keys.iter().take(8).copied().collect();
    let resolves_in = |dir: &Path| -> bool {
        sample.iter().any(|k| {
            let p = Path::new(k);
            if p.is_absolute() {
                p.exists()
            } else {
                dir.join(k).exists()
            }
        })
    };

    if resolves_in(source_root) {
        return source_root.to_path_buf();
    }
    let mut anc = source_root.parent();
    while let Some(p) = anc {
        if resolves_in(p) {
            return p.to_path_buf();
        }
        anc = p.parent();
    }
    // No ancestor resolved any sampled file key. Coverage likely came from a
    // different checkout (e.g., Docker container with `/app/...` paths). The
    // join below will produce non-existent paths that silently won't match
    // mutants — surface this so the failure is diagnosable from logs.
    warn!(
        source_root = %source_root.display(),
        sample = ?sample,
        "coverage file keys do not resolve under source_root or any ancestor; \
         falling back to source_root — all mutants may appear uncovered"
    );
    source_root.to_path_buf()
}

/// Rebase a pytest node id so its path component is relative to
/// `project_root` (the directory pytest will run from in the mirror).
///
/// Node id format: `path/to/test_file.py[::TestClass]::test_name[params]`.
/// The path is everything up to the first `::`. We resolve it against
/// `coverage_cwd`, then strip the canonicalized `project_root` prefix.
/// Returns `None` if the resolved path lies outside `project_root` — those
/// tests can't run from the mirror, so the caller drops them.
fn rebase_node_id(
    id: &str,
    coverage_cwd: &Path,
    canonical_project: &Path,
    cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    let (path_part, rest) = match id.split_once("::") {
        Some((p, r)) => (p, Some(r)),
        None => (id, None),
    };
    let rebased_path = if let Some(hit) = cache.get(path_part) {
        hit.clone()?
    } else {
        let abs = if Path::new(path_part).is_absolute() {
            PathBuf::from(path_part)
        } else {
            coverage_cwd.join(path_part)
        };
        let abs = abs.canonicalize().unwrap_or(abs);
        let computed = abs
            .strip_prefix(canonical_project)
            .ok()
            .and_then(|rel| rel.to_str().map(|s| s.replace('\\', "/")));
        cache.insert(path_part.to_string(), computed.clone());
        computed?
    };
    Some(match rest {
        Some(r) => format!("{rebased_path}::{r}"),
        None => rebased_path,
    })
}

pub struct CoverageFilter {
    ctx: Arc<CoverageContexts>,
}

impl CoverageFilter {
    pub fn new(ctx: Arc<CoverageContexts>) -> Self {
        Self { ctx }
    }
}

impl Filter for CoverageFilter {
    fn name(&self) -> &'static str {
        "coverage"
    }

    fn admits(&self, m: &Mutant) -> Result<bool> {
        match self.ctx.tests_for_mutant(m) {
            Some(tests) if !tests.is_empty() => Ok(true),
            _ => {
                // One uncovered mutant per untested line on large targets would
                // flood logs at `warn`. Drop to `debug`; aggregate counts are
                // already reported by the higher-level filter summary.
                debug!(
                    file = %m.file.display(),
                    line = m.line,
                    stmt_line = m.stmt_line,
                    "no test context for line or its statement head; skipping mutant"
                );
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_doc(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("coverage.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn make_mutant(file: PathBuf, line: u32) -> Mutant {
        crate::mutator::Mutant {
            id: "id".into(),
            file,
            operator: crate::mutator::Operator::ArithOpSwap,
            range: ruff_text_size::TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line,
            stmt_line: line,
        }
    }

    #[test]
    fn parses_contexts_strips_phase_suffix_and_empties() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/test_a.py::test_x|run", "", "tests/test_a.py::test_x|teardown"],
                        "2": [""]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();

        let tests = ctx.tests_for(&py, 1).unwrap();
        assert_eq!(tests, &["tests/test_a.py::test_x".to_string()]);
        // Line 2 only had empty contexts → dropped entirely.
        assert!(ctx.tests_for(&py, 2).is_none());
    }

    #[test]
    fn breadth_map_counts_lines_per_test() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foo.py"), "a = 1\nb = 2\nc = 3\n").unwrap();
        // test_wide covers 3 lines; test_narrow covers 1.
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/t.py::test_wide|run"],
                        "2": ["tests/t.py::test_wide|run", "tests/t.py::test_narrow|run"],
                        "3": ["tests/t.py::test_wide|run"]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let breadth = ctx.breadth_map();
        assert_eq!(breadth.get("tests/t.py::test_wide"), Some(&3));
        assert_eq!(breadth.get("tests/t.py::test_narrow"), Some(&1));
        // A test not in the coverage map has no breadth.
        assert_eq!(breadth.get("tests/t.py::test_absent"), None);
    }

    fn ctx_with_breadth(dir: &Path) -> Arc<CoverageContexts> {
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
        // wide→3 lines, mid→1, narrow→1.
        let path = write_doc(dir, doc);
        CoverageContexts::from_json(&path, dir, dir).unwrap()
    }

    #[test]
    fn order_by_breadth_puts_the_most_specific_test_first() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        // Input in an arbitrary order; wide (breadth 3) must sink, the two
        // breadth-1 tests keep their relative input order (stable).
        let input: Vec<String> = ["tests/t.py::wide", "tests/t.py::narrow", "tests/t.py::mid"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out: Vec<String> = ctx.order_by_breadth(&input).into_iter().cloned().collect();
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
    fn order_by_breadth_unknown_ids_sort_last_and_keep_order() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        // `ghost` isn't in the coverage map → treated as maximally broad → last;
        // `narrow` (breadth 1) leads.
        let input: Vec<String> = ["tests/t.py::ghost", "tests/t.py::narrow"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out: Vec<String> = ctx.order_by_breadth(&input).into_iter().cloned().collect();
        assert_eq!(out[0], "tests/t.py::narrow");
        assert_eq!(out[1], "tests/t.py::ghost");
    }

    #[test]
    fn order_by_breadth_is_always_a_permutation() {
        // Verdict-invariance: ordering only permutes the set. pytest `-x` exits
        // non-zero iff any selected test fails — independent of order — so an
        // identical set is an identical kill/survive verdict.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        for raw in [
            vec![],
            vec!["tests/t.py::wide"],
            vec!["tests/t.py::wide", "tests/t.py::narrow", "tests/t.py::mid"],
            vec!["ghost", "tests/t.py::wide", "dup", "dup"],
        ] {
            let input: Vec<String> = raw.iter().map(|s| s.to_string()).collect();
            let out: Vec<String> = ctx.order_by_breadth(&input).into_iter().cloned().collect();
            let mut a = input.clone();
            let mut b = out.clone();
            a.sort();
            b.sort();
            assert_eq!(a, b, "must be a permutation of {input:?}, got {out:?}");
        }
    }

    #[test]
    fn missing_coverage_file_gives_actionable_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("coverage.json");
        let err = CoverageContexts::from_path(&missing, tmp.path(), tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not found"), "got: {msg}");
        // Names the two generation commands and the opt-out.
        assert!(msg.contains("--cov-context=test"), "got: {msg}");
        assert!(msg.contains("coverage json"), "got: {msg}");
        assert!(msg.contains("--no-coverage"), "got: {msg}");
        // Must NOT leak the bare OS error.
        assert!(!msg.contains("os error"), "leaked raw OS error: {msg}");
    }

    #[test]
    fn rejects_doc_with_no_contexts() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = r#"{"files": { "foo.py": { "contexts": {} } }}"#;
        let path = write_doc(tmp.path(), doc);
        let err = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("no per-test contexts"));
    }

    #[test]
    fn rejects_when_all_contexts_outside_project_root() {
        // Sibling-package layout: project_root is `proj_a/`, but every test
        // node id points at `proj_b/tests/...`. Without the dedicated branch,
        // users get the misleading "regenerate with --show-contexts" message
        // even though the coverage file is well-formed.
        let tmp = tempfile::tempdir().unwrap();
        let proj_a = tmp.path().join("proj_a");
        let proj_b = tmp.path().join("proj_b");
        std::fs::create_dir_all(proj_a.join("src")).unwrap();
        std::fs::create_dir_all(proj_b.join("tests")).unwrap();
        std::fs::write(proj_a.join("src/foo.py"), "x = 1\n").unwrap();
        std::fs::write(proj_b.join("tests/test_a.py"), "def test_x(): pass\n").unwrap();

        let doc = r#"{
            "files": {
                "src/foo.py": {
                    "contexts": {
                        "1": ["../proj_b/tests/test_a.py::test_x|run"]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let err = CoverageContexts::from_json(&path, &proj_a, &proj_a).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fall outside project_root"),
            "unexpected error: {msg}"
        );
        assert!(!msg.contains("Regenerate with"), "wrong branch: {msg}");
    }

    #[test]
    fn filter_admits_covered_skips_uncovered() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let f = CoverageFilter::new(ctx);

        assert!(f.admits(&make_mutant(py.clone(), 1)).unwrap());
        assert!(!f.admits(&make_mutant(py.clone(), 99)).unwrap());
    }

    /// A mutant on a continuation line whose own line carries no context, so a
    /// strict per-line lookup drops it and no test can rescue it.
    fn continuation_mutant(file: PathBuf, line: u32, stmt_line: u32) -> Mutant {
        Mutant {
            stmt_line,
            ..make_mutant(file, line)
        }
    }

    #[test]
    fn filter_admits_mutant_on_continuation_line_of_covered_statement() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        // DEFAULT_ITEMS = [        <- line 1, the head the fold attributes to
        //     "read",             <- lines 2-4, where the mutants land; three
        //     "write",               constant elements, so CPython folds them
        //     "admin",               onto line 1 and they get no context
        // ]
        std::fs::write(
            &py,
            "DEFAULT_ITEMS = [\n    \"read\",\n    \"write\",\n    \"admin\",\n]\n",
        )
        .unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let f = CoverageFilter::new(ctx.clone());

        let m = continuation_mutant(py.clone(), 2, 1);
        assert!(
            f.admits(&m).unwrap(),
            "mutant on a continuation line of a covered statement must not be dropped"
        );
        // Selection must agree with the filter, or the admitted mutant would
        // run against an empty test set.
        assert_eq!(
            ctx.tests_for_mutant(&m).unwrap(),
            &["tests/test_a.py::test_x".to_string()]
        );
    }

    #[test]
    fn own_line_context_wins_over_the_statement_head() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        // Both lines carry contexts, and the head's set is the looser one: a
        // test that short-circuits before the continuation line still records
        // the head. The tighter own-line set must win, or selection silently
        // widens to every test that merely entered the statement.
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/test_a.py::test_head|run",
                              "tests/test_a.py::test_elt|run"],
                        "2": ["tests/test_a.py::test_elt|run"]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();

        let m = continuation_mutant(py.clone(), 2, 1);
        assert_eq!(
            ctx.tests_for_mutant(&m).unwrap(),
            &["tests/test_a.py::test_elt".to_string()],
            "the fallback must not be reached when the mutated line has contexts"
        );
    }

    #[test]
    fn filter_still_skips_when_the_statement_head_is_uncovered() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let f = CoverageFilter::new(ctx.clone());

        // Statement spans lines 40-41; neither is covered.
        let m = continuation_mutant(py.clone(), 41, 40);
        assert!(!f.admits(&m).unwrap());
        assert!(ctx.tests_for_mutant(&m).is_none());
    }

    #[test]
    fn absent_stmt_line_disables_the_fallback() {
        // `stmt_line: 0` is what a report written before the field existed
        // deserializes to. It must behave exactly like a per-line lookup
        // rather than resolving to line 1.
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();

        assert!(ctx
            .tests_for_mutant(&continuation_mutant(py.clone(), 2, 0))
            .is_none());
    }

    #[test]
    fn rebases_node_ids_when_coverage_ran_from_monorepo_root() {
        // Layout: monorepo/proj_a/{foo.py, tests/test_a.py}, coverage run
        // from monorepo/ so keys are `proj_a/foo.py` and node ids are
        // `proj_a/tests/test_a.py::test_x`. project_root = proj_a.
        let tmp = tempfile::tempdir().unwrap();
        let monorepo = tmp.path();
        let proj_a = monorepo.join("proj_a");
        let tests_dir = proj_a.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(proj_a.join("foo.py"), "x = 1\n").unwrap();
        std::fs::write(tests_dir.join("test_a.py"), "def test_x(): pass\n").unwrap();
        // pyproject.toml is what find_project_root keys on, but from_json
        // takes project_root directly — drop it so the test is hermetic.

        let doc = r#"{
            "files": {
                "proj_a/foo.py": {
                    "contexts": { "1": ["proj_a/tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(monorepo, doc);
        let ctx = CoverageContexts::from_json(&path, &proj_a, &proj_a).unwrap();

        let tests = ctx.tests_for(&proj_a.join("foo.py"), 1).unwrap();
        assert_eq!(tests, &["tests/test_a.py::test_x".to_string()]);
    }

    #[test]
    fn drops_node_ids_outside_project_root() {
        // Tests that live outside the project (sibling package in a monorepo)
        // can't be invoked from inside the mirror — drop them.
        let tmp = tempfile::tempdir().unwrap();
        let monorepo = tmp.path();
        let proj_a = monorepo.join("proj_a");
        let proj_b_tests = monorepo.join("proj_b/tests");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b_tests).unwrap();
        std::fs::write(proj_a.join("foo.py"), "x = 1\n").unwrap();
        std::fs::write(proj_b_tests.join("test_b.py"), "def test_y(): pass\n").unwrap();

        let doc = r#"{
            "files": {
                "proj_a/foo.py": {
                    "contexts": {
                        "1": [
                            "proj_a/tests/test_a.py::test_x|run",
                            "proj_b/tests/test_b.py::test_y|run"
                        ]
                    }
                }
            }
        }"#;
        // Only the proj_a test should survive; proj_b test lives outside
        // project_root and pytest in the mirror can't reach it.
        std::fs::create_dir_all(proj_a.join("tests")).unwrap();
        std::fs::write(proj_a.join("tests/test_a.py"), "def test_x(): pass\n").unwrap();

        let path = write_doc(monorepo, doc);
        let ctx = CoverageContexts::from_json(&path, &proj_a, &proj_a).unwrap();
        let tests = ctx.tests_for(&proj_a.join("foo.py"), 1).unwrap();
        assert_eq!(tests, &["tests/test_a.py::test_x".to_string()]);
    }

    #[test]
    fn handles_source_root_below_project_root() {
        // source_root is `proj/src/pkg` but project_root is `proj/`, and
        // coverage was run from `proj/`. Detection walks up from source_root
        // until file keys resolve, finding `proj/`.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        let pkg = proj.join("src/pkg");
        let tests = proj.join("tests");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(pkg.join("foo.py"), "x = 1\n").unwrap();
        std::fs::write(tests.join("test_a.py"), "def test_x(): pass\n").unwrap();

        let doc = r#"{
            "files": {
                "src/pkg/foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(proj, doc);
        let ctx = CoverageContexts::from_json(&path, &pkg, proj).unwrap();

        let tests_for = ctx.tests_for(&pkg.join("foo.py"), 1).unwrap();
        assert_eq!(tests_for, &["tests/test_a.py::test_x".to_string()]);
    }

    /// Encode line numbers into a coverage.py numbits blob (inverse of
    /// `numbits_to_lines`), for building test fixtures.
    fn lines_to_numbits(lines: &[u32]) -> Vec<u8> {
        let max = lines.iter().copied().max().unwrap_or(0);
        let mut blob = vec![0u8; (max as usize / 8) + 1];
        for &n in lines {
            blob[n as usize / 8] |= 1 << (n % 8);
        }
        blob
    }

    /// Write a minimal coverage.py v7 SQLite db with the given
    /// (file_path, context, lines) rows.
    fn write_cov_db(path: &Path, rows: &[(&str, &str, &[u32])]) {
        use rusqlite::Connection;
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE file (id integer primary key, path text, unique(path));
             CREATE TABLE context (id integer primary key, context text, unique(context));
             CREATE TABLE line_bits (file_id integer, context_id integer, numbits blob,
                 unique(file_id, context_id));",
        )
        .unwrap();
        let mut next_file = 1i64;
        let mut next_ctx = 1i64;
        let mut files: HashMap<&str, i64> = HashMap::new();
        let mut ctxs: HashMap<&str, i64> = HashMap::new();
        for (file, ctx, lines) in rows {
            let fid = *files.entry(file).or_insert_with(|| {
                let id = next_file;
                next_file += 1;
                conn.execute("INSERT INTO file (id, path) VALUES (?1, ?2)", (id, file))
                    .unwrap();
                id
            });
            let cid = *ctxs.entry(ctx).or_insert_with(|| {
                let id = next_ctx;
                next_ctx += 1;
                conn.execute(
                    "INSERT INTO context (id, context) VALUES (?1, ?2)",
                    (id, ctx),
                )
                .unwrap();
                id
            });
            conn.execute(
                "INSERT INTO line_bits (file_id, context_id, numbits) VALUES (?1, ?2, ?3)",
                (fid, cid, lines_to_numbits(lines)),
            )
            .unwrap();
        }
    }

    #[test]
    fn numbits_roundtrips() {
        for lines in [vec![0u32], vec![1, 7, 8, 9, 22, 23, 31], vec![100, 255]] {
            assert_eq!(numbits_to_lines(&lines_to_numbits(&lines)), lines);
        }
    }

    #[test]
    fn sqlite_sniffer_distinguishes_db_from_json() {
        let tmp = tempfile::tempdir().unwrap();
        let json = tmp.path().join("coverage.json");
        std::fs::write(&json, r#"{"files": {}}"#).unwrap();
        assert!(!is_sqlite(&json).unwrap());

        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();
        let db = tmp.path().join(".coverage");
        write_cov_db(&db, &[("foo.py", "t.py::test_x|run", &[1])]);
        assert!(is_sqlite(&db).unwrap());
    }

    #[test]
    fn reads_coverage_sqlite_db() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\ny = 2\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
        std::fs::write(tmp.path().join("tests/test_a.py"), "def test_x(): pass\n").unwrap();

        let db = tmp.path().join(".coverage");
        write_cov_db(
            &db,
            &[
                ("foo.py", "tests/test_a.py::test_x|run", &[1]),
                ("foo.py", "tests/test_a.py::test_x|setup", &[1]),
                ("foo.py", "", &[2]), // import-time context, dropped
            ],
        );

        // from_path sniffs SQLite and routes to from_coverage_db.
        let ctx = CoverageContexts::from_path(&db, tmp.path(), tmp.path()).unwrap();
        assert_eq!(
            ctx.tests_for(&py, 1).unwrap(),
            &["tests/test_a.py::test_x".to_string()]
        );
        // Line 2 only had the empty import-time context -> no selectable test.
        assert!(ctx.tests_for(&py, 2).is_none());
    }

    #[test]
    fn dedupes_repeated_test_ids_across_phases() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": [
                            "tests/test_a.py::test_x|run",
                            "tests/test_a.py::test_x|setup",
                            "tests/test_a.py::test_x|teardown"
                        ]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        assert_eq!(ctx.tests_for(&py, 1).unwrap().len(), 1);
    }
}
