//! ty pre-filter.
//!
//! For each mutant we materialize the patched source into a temp file that
//! shadows the original (via `--python-path` overlay or a sandboxed copy of the
//! project) and run `ty check` on it. If the patched file introduces a type
//! error that the original did not have, the mutant is dropped.
//!
//! v1: subprocess invocation. v2: embed via `ty_python_semantic` once that
//! crate stabilizes a public API.
//!
//! ## Result cache
//!
//! Each `ty check` invocation costs ~150-200ms (cold typeshed bootstrap +
//! parse + inference). Across thousands of mutants this dominates wall-time
//! — instrumentation on real projects measured ty as 96-97% of total CPU.
//! [`TyFilter`] persists every admit/reject verdict to
//! `.fermut/ty-cache.json` keyed on `(ty_version, ast_hash(file),
//! range, replacement)` so subsequent runs short-circuit unchanged
//! mutants entirely.
//!
//! Cache safety: the AST hash invalidates on any structural source change;
//! the ty version invalidates on `ty` binary upgrade; the range +
//! replacement strings disambiguate sibling mutants in the same file. A
//! cache hit is a *deterministic* substitute for re-running ty.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use super::ty_embedded::EmbeddedTyChecker;
use super::Filter;
use crate::ast_hash;
use crate::emit::patch_source;
use crate::mutator::Mutant;
use crate::sync::lock_recover;

const TY_CACHE_FILENAME: &str = "ty-cache.json";
const FERMUT_DIR: &str = ".fermut";

pub struct TyFilter {
    /// `ty` binary path. `None` when the embedded pool fully owns the
    /// dispatch path and no subprocess fallback is reachable.
    bin: Option<String>,
    bin_version: String,
    baseline: Mutex<HashMap<PathBuf, usize>>,
    ast_hashes: Mutex<HashMap<PathBuf, String>>,
    cache: Mutex<TyCache>,
    cache_path: Option<PathBuf>,
    /// Per-worker pool of in-process ty checkers. Each rayon worker
    /// thread grabs its own slot, so `check_patched` calls run in
    /// parallel instead of serializing through one shared checker.
    /// `None` means the subprocess fallback is in use.
    embedded: Option<EmbeddedPool>,
}

/// Pool of one [`EmbeddedTyChecker`] per worker. Slots are filled
/// lazily on first access — workers that never run pay nothing.
/// Each slot owns its own `ProjectDatabase` so salsa-level mutations
/// don't cross-talk between workers.
struct EmbeddedPool {
    project_path: PathBuf,
    slots: Vec<Mutex<Option<EmbeddedTyChecker>>>,
}

impl EmbeddedPool {
    /// Build a pool with `seed` already installed in slot 0. The seed
    /// is the checker produced by the construction-time smoke build,
    /// reused here so its typeshed bootstrap isn't paid twice.
    fn new(project_path: PathBuf, size: usize, seed: EmbeddedTyChecker) -> Self {
        let size = size.max(1);
        let mut slots = Vec::with_capacity(size);
        slots.push(Mutex::new(Some(seed)));
        for _ in 1..size {
            slots.push(Mutex::new(None));
        }
        Self {
            project_path,
            slots,
        }
    }

    /// Borrow this worker's checker (building it on first use) and
    /// hand it to `f`. Selects a slot by rayon worker index when
    /// called from inside a rayon pool; otherwise picks slot 0 (which
    /// just means non-rayon calls serialize through one checker).
    fn with<R>(&self, f: impl FnOnce(&mut EmbeddedTyChecker) -> Result<R>) -> Result<R> {
        let idx = rayon::current_thread_index().unwrap_or(0) % self.slots.len();
        let mut guard = self.slots[idx]
            .lock()
            .map_err(|_| anyhow!("embedded ty slot mutex poisoned"))?;
        if guard.is_none() {
            *guard = Some(EmbeddedTyChecker::new(&self.project_path).with_context(|| {
                format!(
                    "lazily building embedded ty checker for slot {idx} at {}",
                    self.project_path.display()
                )
            })?);
        }
        f(guard.as_mut().expect("slot was just populated"))
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
struct TyCache {
    /// Schema version. Bump if the key shape or value type changes so
    /// older caches are silently dropped instead of mis-keyed.
    #[serde(default)]
    schema: String,
    /// ty version the cache was built under. Mixed into the key already,
    /// but also recorded here so a human inspecting the JSON can see
    /// which ty release produced the verdicts.
    #[serde(default)]
    ty_version: String,
    /// key (hex sha256) -> admits-the-mutant
    #[serde(default)]
    entries: HashMap<String, bool>,
}

const CACHE_SCHEMA: &str = "ty-cache:v1";

impl TyFilter {
    /// Construct a `TyFilter` that prefers a pool of in-process
    /// [`EmbeddedTyChecker`]s rooted at `project_path` (one per
    /// worker, sized to `pool_size`). Falls back to the subprocess
    /// path when the embedded checker fails to build (the `ty_project`
    /// crate is pre-release, so its construction surface can shift).
    ///
    /// `FERMUT_TY_EMBEDDED=0` forces the subprocess path for debugging.
    pub fn with_cache_and_project(
        cache_path: Option<PathBuf>,
        project_path: &Path,
        pool_size: usize,
    ) -> Result<Self> {
        let embedded = if env_disables_embedded() {
            tracing::debug!("ty embedded path disabled via FERMUT_TY_EMBEDDED=0");
            None
        } else {
            // Smoke-build a single checker now so we surface fatal
            // configuration errors (e.g. malformed pyproject) at filter
            // construction time, before any per-mutant work starts. The
            // built checker seeds slot 0 of the pool so its typeshed
            // bootstrap isn't repeated when the first worker runs.
            match EmbeddedTyChecker::new(project_path) {
                Ok(seed) => Some(EmbeddedPool::new(
                    project_path.to_path_buf(),
                    pool_size,
                    seed,
                )),
                Err(e) => {
                    warn!(
                        project = %project_path.display(),
                        error = %e,
                        "embedded ty checker unavailable; falling back to subprocess"
                    );
                    None
                }
            }
        };
        Self::build(cache_path, embedded, project_path)
    }

    fn build(
        cache_path: Option<PathBuf>,
        embedded: Option<EmbeddedPool>,
        scope: &Path,
    ) -> Result<Self> {
        // When the embedded pool is in use, the `ty` binary is only
        // needed as a subprocess fallback for non-embedded code paths
        // (e.g. baseline counts via `error_count`). The embedded path
        // itself never shells out, so a missing binary is recoverable:
        // we record it as None and any subprocess attempt will surface
        // its own error. When embedded is disabled the binary is hard-
        // required.
        let bin = match which_ty(scope) {
            Ok(b) => Some(b),
            Err(e) => {
                if embedded.is_none() {
                    return Err(e).context(
                        "ty binary not found; install from astral-sh/ty or pass --no-ty-filter",
                    );
                }
                None
            }
        };
        // Bake the dispatch mode into the cache key so embedded-mode
        // verdicts can't satisfy a subprocess-mode lookup (or vice
        // versa) — minor diagnostic divergence between paths would
        // otherwise produce silent miscaches.
        let bin_version = match (&bin, embedded.is_some()) {
            (Some(b), true) => format!("{}+embedded", probe_version(b)),
            (Some(b), false) => probe_version(b),
            (None, true) => format!("{}+embedded-only", env!("CARGO_PKG_VERSION")),
            (None, false) => unreachable!("bin None requires embedded Some — see early return"),
        };
        let cache = cache_path
            .as_deref()
            .map(load_cache)
            .unwrap_or_default()
            .invalidate_on_mismatch(&bin_version);
        Ok(Self {
            bin,
            bin_version,
            baseline: Mutex::new(HashMap::new()),
            ast_hashes: Mutex::new(HashMap::new()),
            cache: Mutex::new(cache),
            cache_path,
            embedded,
        })
    }

    /// Default cache location under a project root.
    pub fn default_cache_path(source_root: &Path) -> PathBuf {
        source_root.join(FERMUT_DIR).join(TY_CACHE_FILENAME)
    }

    /// `Ok(None)` when a subprocess `ty check` timed out — the caller treats
    /// that as a filter-bypass (admit the mutant) rather than hanging the run.
    fn check(&self, mutant: &Mutant) -> Result<Option<bool>> {
        let Some(baseline_count) = self.baseline_count_for(&mutant.file)? else {
            return Ok(None);
        };
        let Some(mutant_count) = self.mutant_error_count(mutant)? else {
            return Ok(None);
        };
        Ok(Some(mutant_count <= baseline_count))
    }

    fn mutant_error_count(&self, mutant: &Mutant) -> Result<Option<usize>> {
        if let Some(pool) = &self.embedded {
            let original = std::fs::read_to_string(&mutant.file)
                .with_context(|| format!("reading {}", mutant.file.display()))?;
            let patched = patch_source(&original, mutant.range, &mutant.replacement);
            return pool
                .with(|c| c.check_patched(&mutant.file, &patched))
                .map(Some);
        }
        // Subprocess fallback: patch to a tempfile, run `ty check`.
        let original = std::fs::read_to_string(&mutant.file)
            .with_context(|| format!("reading {}", mutant.file.display()))?;
        let patched = patch_source(&original, mutant.range, &mutant.replacement);
        let tmp = super::patched_tempfile(&mutant.file, &patched)?;
        self.error_count(tmp.path().to_string_lossy().as_ref())
    }

    fn baseline_count_for(&self, file: &PathBuf) -> Result<Option<usize>> {
        if let Some(&n) = lock_recover(&self.baseline).get(file) {
            return Ok(Some(n));
        }
        let n = if let Some(pool) = &self.embedded {
            pool.with(|c| c.error_count(file))?
        } else {
            match self.error_count(file.to_string_lossy().as_ref())? {
                Some(n) => n,
                None => return Ok(None),
            }
        };
        lock_recover(&self.baseline).insert(file.clone(), n);
        Ok(Some(n))
    }

    /// `Ok(None)` when the `ty` subprocess outran the filter timeout.
    fn error_count(&self, path: &str) -> Result<Option<usize>> {
        let bin = self.bin.as_deref().ok_or_else(|| {
            anyhow!(
                "ty subprocess path requested but ty binary not on PATH \
                 (embedded mode is active; this only fires if a code path \
                 forgets to use the embedded pool)"
            )
        })?;
        let mut cmd = Command::new(bin);
        // `gitlab` is a structured JSON array — each diagnostic carries a
        // `severity` field, so we count real diagnostics instead of substring-
        // matching `": error["` against human output (where a message body
        // echoing that substring double-counts and flips the verdict).
        cmd.args(["check", "--output-format", "gitlab", path]);
        let Some(out) = super::run_filter_with_timeout(cmd)
            .with_context(|| format!("invoking `{} check {}`", bin, path))?
        else {
            warn!(
                path,
                timeout_secs = super::FILTER_SUBPROCESS_TIMEOUT.as_secs(),
                "ty check timed out; bypassing ty filter for this file"
            );
            return Ok(None);
        };
        // ty exits non-zero when there are diagnostics; we count, not gate on, exit code.
        Ok(count_error_diagnostics(&out.stdout))
    }

    /// Memoized AST hash for the file containing `mutant`. Falls back to an
    /// error key when reading fails, which causes the cache to silently
    /// miss for that file (per-call lookups still run ty itself).
    fn ast_hash_for(&self, file: &Path) -> Option<String> {
        if let Some(h) = lock_recover(&self.ast_hashes).get(file) {
            return Some(h.clone());
        }
        match ast_hash::hash_file_ast(file) {
            Ok(h) => {
                lock_recover(&self.ast_hashes).insert(file.to_path_buf(), h.clone());
                Some(h)
            }
            Err(e) => {
                warn!(file = %file.display(), error = %e, "ty cache: ast-hash failed; bypassing cache for this file");
                None
            }
        }
    }

    fn cache_key(&self, file_ast_hash: &str, mutant: &Mutant) -> String {
        let mut h = Sha256::new();
        h.update(CACHE_SCHEMA.as_bytes());
        h.update(b"|");
        h.update(self.bin_version.as_bytes());
        h.update(b"|");
        h.update(file_ast_hash.as_bytes());
        h.update(b"|");
        h.update(u32::from(mutant.range.start()).to_le_bytes());
        h.update(b"|");
        h.update(u32::from(mutant.range.end()).to_le_bytes());
        h.update(b"|");
        h.update(mutant.replacement.as_bytes());
        hex::encode(h.finalize())
    }

    /// Save the cache to disk. Idempotent; safe to call from `Drop`. Errors
    /// are logged but never propagated — losing a cache update is strictly
    /// better than poisoning the run.
    fn persist(&self) {
        let Some(path) = &self.cache_path else { return };
        let guard = match self.cache.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = save_cache(path, &guard) {
            warn!(path = %path.display(), error = %e, "ty cache: save failed");
        }
    }
}

impl Filter for TyFilter {
    fn name(&self) -> &'static str {
        "ty"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        if let Some(file_hash) = self.ast_hash_for(&mutant.file) {
            let key = self.cache_key(&file_hash, mutant);
            if let Some(&hit) = lock_recover(&self.cache).entries.get(&key) {
                return Ok(hit);
            }
            // A timeout bypass (None) is deliberately *not* cached — the hang is
            // transient, so a later run should get a real chance to type-check.
            match self.check(mutant)? {
                Some(admits) => {
                    lock_recover(&self.cache).entries.insert(key, admits);
                    Ok(admits)
                }
                None => Ok(true),
            }
        } else {
            // No AST hash → skip caching but still run ty.
            Ok(self.check(mutant)?.unwrap_or(true))
        }
    }
}

impl Drop for TyFilter {
    fn drop(&mut self) {
        self.persist();
    }
}

impl TyCache {
    /// Discard entries when the recorded schema or ty version doesn't match
    /// the running binary. Re-keying is cheaper than risking a stale verdict
    /// from a different ty release.
    fn invalidate_on_mismatch(mut self, current_version: &str) -> Self {
        if self.schema != CACHE_SCHEMA || self.ty_version != current_version {
            self.schema = CACHE_SCHEMA.to_string();
            self.ty_version = current_version.to_string();
            self.entries.clear();
        }
        self
    }
}

fn load_cache(path: &Path) -> TyCache {
    // Route through the shared loader so a corrupt cache is quarantined and
    // warned about, not silently reset to zero verdicts (a full ty re-run —
    // the most expensive filter — with no trace of why).
    crate::cache::load_or_quarantine(path)
}

fn save_cache(path: &Path, cache: &TyCache) -> Result<()> {
    let raw = serde_json::to_string_pretty(cache).context("serializing ty cache")?;
    crate::cache::atomic_write(path, raw)
}

/// True when the user has set `FERMUT_TY_EMBEDDED=0`. Any other value
/// (including unset, empty, or `1`) leaves the embedded path enabled.
fn env_disables_embedded() -> bool {
    std::env::var("FERMUT_TY_EMBEDDED").ok().as_deref() == Some("0")
}

fn probe_version(bin: &str) -> String {
    let out = Command::new(bin).arg("--version").output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// One entry of ty's `--output-format gitlab` JSON array. Only the severity
/// matters here.
#[derive(Deserialize)]
struct GitlabDiagnostic {
    severity: String,
}

/// Count error-severity diagnostics in ty's gitlab JSON. ruff_db's gitlab
/// emitter maps ty `Error`→"major" and `Fatal`→"critical" (Warning→"minor",
/// Info→"info"), so counting `major`/`critical` mirrors the embedded path's
/// `Severity::Error | Severity::Fatal` filter exactly — the two ty backends
/// agree on what counts as a new type error.
///
/// Returns `None` when stdout isn't the expected JSON array (e.g. ty crashed
/// mid-write); the caller then bypasses the filter rather than gating on a
/// count it can't trust — same conservative direction as a timeout.
fn count_error_diagnostics(stdout: &[u8]) -> Option<usize> {
    let diags: Vec<GitlabDiagnostic> = serde_json::from_slice(stdout).ok()?;
    Some(
        diags
            .iter()
            .filter(|d| d.severity == "major" || d.severity == "critical")
            .count(),
    )
}

/// Resolve the `ty` binary, preferring the project's venv `bin/` (derived from
/// `scope`), then the ambient PATH — the same resolution `doctor` reports, so a
/// green `ty ok` there means a real run finds it too. Portable across Windows
/// (no `which` binary there) via the shared [`crate::runner::resolve_tool`].
fn which_ty(scope: &Path) -> Result<String> {
    crate::runner::resolve_tool("ty", scope)
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("`ty` not found in the project venv or on PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::{TextRange, TextSize};

    fn dummy_mutant(file: &Path) -> Mutant {
        Mutant {
            id: "t".into(),
            file: file.to_path_buf(),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(TextSize::from(0), TextSize::from(1)),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn cache_default_path_under_dot_fermut() {
        let p = TyFilter::default_cache_path(Path::new("/proj"));
        assert!(p.ends_with(Path::new(".fermut").join("ty-cache.json")));
    }

    #[test]
    fn count_error_diagnostics_counts_major_and_critical_only() {
        let json = r#"[
            {"severity": "major",    "description": "e1"},
            {"severity": "critical", "description": "e2"},
            {"severity": "minor",    "description": "a warning"},
            {"severity": "info",     "description": "note"}
        ]"#;
        assert_eq!(count_error_diagnostics(json.as_bytes()), Some(2));
    }

    #[test]
    fn count_error_diagnostics_empty_array_is_zero() {
        assert_eq!(count_error_diagnostics(b"[]"), Some(0));
    }

    #[test]
    fn count_error_diagnostics_ignores_substring_in_message() {
        // Regression: a diagnostic message echoing `: error[` must not inflate
        // the count — structured severity is the only signal.
        let json = r#"[
            {"severity": "major", "description": "unexpected token near: error[code]: oops"}
        ]"#;
        assert_eq!(count_error_diagnostics(json.as_bytes()), Some(1));
    }

    #[test]
    fn count_error_diagnostics_bad_json_is_none() {
        assert_eq!(count_error_diagnostics(b"ty panicked"), None);
    }

    #[test]
    fn load_cache_quarantines_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ty-cache.json");
        std::fs::write(&path, "{ not json").unwrap();

        let c = load_cache(&path);
        assert!(c.entries.is_empty());
        // Corrupt bytes moved aside, not silently overwritten on next save.
        assert!(!path.exists());
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("ty-cache.corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "expected one quarantine file");
    }

    #[test]
    fn cache_invalidates_on_version_mismatch() {
        let mut c = TyCache {
            schema: CACHE_SCHEMA.into(),
            ty_version: "old".into(),
            entries: HashMap::from([("k".to_string(), true)]),
        };
        c = c.invalidate_on_mismatch("new");
        assert!(c.entries.is_empty(), "entries must clear on version bump");
        assert_eq!(c.ty_version, "new");
    }

    #[test]
    fn cache_invalidates_on_schema_mismatch() {
        let mut c = TyCache {
            schema: "ty-cache:v0".into(),
            ty_version: "v".into(),
            entries: HashMap::from([("k".to_string(), false)]),
        };
        c = c.invalidate_on_mismatch("v");
        assert!(c.entries.is_empty());
        assert_eq!(c.schema, CACHE_SCHEMA);
    }

    #[test]
    fn cache_keeps_entries_when_versions_match() {
        let mut c = TyCache {
            schema: CACHE_SCHEMA.into(),
            ty_version: "v".into(),
            entries: HashMap::from([("k".to_string(), true)]),
        };
        c = c.invalidate_on_mismatch("v");
        assert_eq!(c.entries.get("k"), Some(&true));
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ty-cache.json");
        let c = TyCache {
            schema: CACHE_SCHEMA.into(),
            ty_version: "v1".into(),
            entries: HashMap::from([("a".to_string(), true), ("b".to_string(), false)]),
        };
        save_cache(&path, &c).unwrap();
        let loaded = load_cache(&path);
        assert_eq!(loaded.entries.get("a"), Some(&true));
        assert_eq!(loaded.entries.get("b"), Some(&false));
        assert_eq!(loaded.ty_version, "v1");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let c = load_cache(Path::new("/does/not/exist/ty-cache.json"));
        assert!(c.entries.is_empty());
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested/.fermut/ty-cache.json");
        let c = TyCache::default();
        save_cache(&nested, &c).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn cache_key_changes_with_replacement() {
        // We can't actually instantiate TyFilter without ty, but we can
        // exercise the key formula via a hand-rolled equivalent.
        fn key(ver: &str, hash: &str, m: &Mutant) -> String {
            let mut h = Sha256::new();
            h.update(CACHE_SCHEMA.as_bytes());
            h.update(b"|");
            h.update(ver.as_bytes());
            h.update(b"|");
            h.update(hash.as_bytes());
            h.update(b"|");
            h.update(u32::from(m.range.start()).to_le_bytes());
            h.update(b"|");
            h.update(u32::from(m.range.end()).to_le_bytes());
            h.update(b"|");
            h.update(m.replacement.as_bytes());
            hex::encode(h.finalize())
        }
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut m = dummy_mutant(tmp.path());
        let k_minus = key("v", "h", &m);
        m.replacement = "/".into();
        let k_slash = key("v", "h", &m);
        assert_ne!(k_minus, k_slash);
    }
}
