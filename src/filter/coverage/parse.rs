//! Ingest: read a coverage file (JSON export or `.coverage` SQLite DB) and
//! build the per-file, per-line test-context index the filter and ordering
//! consult. Auto-detects format, rebases pytest node ids onto `project_root`,
//! and computes the per-test breadth maps.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::warn;

use super::CoverageContexts;

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
        let coverage_cwd = detect_coverage_cwd(source_root, project_root, &file_keys);
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

        // Per-test breadth: count the `(file, line)` cells each node id covers,
        // both globally and per file. One pass over the built map — cheap, done
        // once at load. The per-file counts drive smart ordering; the global
        // counts are the tie-breaker.
        let mut test_breadth: HashMap<String, u32> = HashMap::new();
        let mut file_test_breadth: HashMap<PathBuf, HashMap<String, u32>> = HashMap::new();
        for (path, per_line) in &map {
            let per_file = file_test_breadth.entry(path.clone()).or_default();
            for ids in per_line.values() {
                for id in ids {
                    *test_breadth.entry(id.clone()).or_insert(0) += 1;
                    *per_file.entry(id.clone()).or_insert(0) += 1;
                }
            }
        }

        Ok(Arc::new(Self {
            map,
            canonical_cache: RwLock::new(HashMap::new()),
            test_breadth,
            file_test_breadth,
        }))
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
pub(super) fn numbits_to_lines(blob: &[u8]) -> Vec<u32> {
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
pub(super) fn is_sqlite(path: &Path) -> Result<bool> {
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
/// recorded file keys. Coverage.py records *relative* paths relative to that
/// cwd, so the lowest ancestor of `source_root` where the keys resolve on disk
/// is our answer. When keys are *absolute* (the coverage.py default, and what
/// `.coverage` SQLite stores) they can't discriminate a cwd, so we return
/// `project_root` — pytest's rootdir, which its recorded node ids are relative
/// to. Falls back to `source_root` if there are no keys or no relative key
/// resolves.
pub(super) fn detect_coverage_cwd(
    source_root: &Path,
    project_root: &Path,
    keys: &[&str],
) -> PathBuf {
    if keys.is_empty() {
        return source_root.to_path_buf();
    }
    // 8 is enough to disambiguate any realistic project layout while staying
    // cheap. A real mismatch shows up on the first key; we sample a few more
    // only so a single oddly-named file doesn't dominate the decision.
    //
    // Only *relative* file keys discriminate the coverage cwd: joining a
    // relative key onto a candidate dir either resolves to a real file or it
    // doesn't. An *absolute* key (coverage.py with `relative_files` off — the
    // default, and what `.coverage` SQLite stores) `exists()` no matter which
    // dir we probe, so it would falsely "resolve" under the first candidate
    // (`source_root`) and hand back the wrong cwd — node ids would then get a
    // spurious `src/` prefix, pytest collects nothing, and every mutant is
    // falsely killed (vacuous 100%). So probe with relative keys only.
    let sample: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|k| Path::new(k).is_relative())
        .take(8)
        .collect();
    if sample.is_empty() {
        // All keys absolute → nothing to probe. Pytest node ids are recorded
        // relative to pytest's rootdir, which in the mirror is `project_root`,
        // so that is the cwd node ids resolve against.
        return project_root.to_path_buf();
    }
    let resolves_in = |dir: &Path| -> bool { sample.iter().any(|k| dir.join(k).exists()) };

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
