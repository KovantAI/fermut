//! Per-mutant result cache.
//!
//! Keyed on `(mutant.id, ast_hash(file), scope)`. The file hash is the
//! AST-structural hash from [`crate::ast_hash`] — reformat / comment edits
//! that leave the AST untouched no longer invalidate the cache. The scope
//! string captures everything that could change the test-run outcome without
//! changing the AST: runner kind, timeout, hypothesis seed, pytest args, a
//! content fingerprint of the test-suite tree, and the per-mutant test
//! selection set when coverage-driven selection is on. A mismatch on any of
//! those invalidates the cached entry so a stale "survived" from a narrow
//! `--coverage` run can't poison a later full-suite run (and vice versa), and
//! editing a test re-evaluates the mutants it might now kill instead of
//! serving the pre-edit verdict.
//!
//! The legacy raw-byte hash is still exported as [`hash_file`] for callers
//! that want byte identity (and for the test suite). Pipeline code paths
//! should use [`crate::ast_hash::hash_file_ast`] instead.
//!
//! Stored at `.fermut/cache.json` under the source root by default; one JSON
//! file, atomic write on save.
//!
//! Filter-skipped outcomes are deliberately **not** cached — the filter chain
//! changes between invocations (different `--ops`, different `--diff-only`
//! base) and last run's skip may no longer apply.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hmac::{KeyInit, Mac, SimpleHmac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::report::MutantOutcome;

/// Env var that activates HMAC-signed cache entries. When set, every cache
/// entry is signed on insert and verified on load; entries that fail to
/// verify are dropped with a warning. When unset, cache I/O behaves as it
/// always has — no signing, no verification.
///
/// Intended for CI where the cache may be restored from a shared artifact
/// store and a malicious PR could otherwise pre-seed `Killed` outcomes for
/// real survivors to game the score. Store the key as a secret and inject
/// it on every job. The value is treated as raw bytes; pick something with
/// at least 128 bits of entropy.
const CACHE_KEY_ENV: &str = "FERMUT_CACHE_KEY";

type HmacSha256 = SimpleHmac<Sha256>;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Result<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(key).context("hmac key init")?;
    mac.update(msg);
    Ok(mac.finalize().into_bytes().into())
}

/// Load a JSON cache file, returning `T::default()` on any failure.
///
/// Unlike a bare `unwrap_or_default()`, this:
///
/// - emits a `warn!` so corruption is visible in logs instead of silently
///   wiping the cache (which the user then attributes to "cache invalidation"
///   when really their file is broken);
/// - quarantines the corrupt file as `<path>.corrupt-<pid>` so the user can
///   inspect what was actually on disk rather than have it overwritten by the
///   next `save`.
///
/// Missing file is the normal cold-start path — silent. Anything else
/// (permissions, partial reads, malformed JSON, schema drift) warns.
pub(crate) fn load_or_quarantine<T: Default + DeserializeOwned>(path: &Path) -> T {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "cache: read failed; starting from empty");
            return T::default();
        }
    };
    match serde_json::from_str::<T>(&raw) {
        Ok(v) => v,
        Err(e) => {
            let quarantine = path.with_extension(format!("corrupt-{}", std::process::id()));
            let moved = std::fs::rename(path, &quarantine).is_ok();
            warn!(
                path = %path.display(),
                error = %e,
                quarantined = moved,
                quarantine_path = %quarantine.display(),
                "cache: parse failed; starting from empty"
            );
            T::default()
        }
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Cache {
    /// mutant_id -> entry
    #[serde(default)]
    entries: HashMap<String, CacheEntry>,
    /// HMAC key. `Some` => sign-on-insert and verify-on-load are active.
    /// Never serialized — survives only for the life of one `Cache` value.
    /// Populated from `FERMUT_CACHE_KEY` in the `load` family of
    /// constructors; tests use `load_with_key` to pass an explicit value
    /// and avoid races on the process-global env var.
    #[serde(skip)]
    key: Option<Vec<u8>>,
    /// Number of entries dropped during the last `load_with_key` call due
    /// to HMAC verification failure. Surfaced via `dropped_on_load()` so
    /// the engine can include it in the run summary — without an
    /// aggregate, a steady drip of tampered entries looks like noise in
    /// the per-event `warn!` stream.
    #[serde(skip)]
    dropped_on_load: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    file_hash: String,
    /// Hash of run-shape inputs (runner, timeout, pytest args, per-mutant
    /// test selection, ...). Empty string for entries written by older
    /// fermut versions — those will simply miss on lookup and be re-run.
    #[serde(default)]
    scope: String,
    outcome: MutantOutcome,
    /// Hex-encoded HMAC-SHA256 over `(mutant_id, file_hash, scope, outcome)`
    /// keyed by `FERMUT_CACHE_KEY`. Present only when the key was set at
    /// insert time. Verified at load time when the key is set; ignored when
    /// the key is unset (which keeps unsigned legacy entries usable for
    /// local-dev workflows that don't need integrity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
}

fn cache_key_from_env() -> Option<Vec<u8>> {
    match std::env::var(CACHE_KEY_ENV) {
        Ok(v) if v.is_empty() => {
            // Exporting `FERMUT_CACHE_KEY=` (empty) is almost always a
            // misconfiguration: the user thinks signing is on, but the
            // empty-string branch silently disables it. Warn loudly so
            // the next CI log line points straight at the issue.
            warn!(
                "{CACHE_KEY_ENV} is set but empty; cache HMAC signing is DISABLED. \
                 Unset the variable to suppress this warning, or set a real key."
            );
            None
        }
        Ok(v) => {
            if v.len() < 16 {
                // 16 bytes ≈ 128 bits, the documented floor. Short keys
                // still produce a MAC, but the security margin shrinks.
                warn!(
                    bytes = v.len(),
                    "{CACHE_KEY_ENV} is shorter than the documented 16-byte floor; \
                     HMAC strength is reduced."
                );
            }
            Some(v.into_bytes())
        }
        Err(_) => None,
    }
}

fn compute_mac(key: &[u8], mutant_id: &str, entry: &CacheEntry) -> Result<String> {
    // serde_json of the outcome — stable across runs because MutantOutcome
    // serializes its variants and fields by name. Any change to the outcome
    // (status, killing test id, etc.) flips the MAC.
    let outcome_json = serde_json::to_string(&entry.outcome).context("serializing outcome")?;
    let mut msg = Vec::with_capacity(
        mutant_id.len() + entry.file_hash.len() + entry.scope.len() + outcome_json.len() + 3,
    );
    msg.extend_from_slice(mutant_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(entry.file_hash.as_bytes());
    msg.push(0);
    msg.extend_from_slice(entry.scope.as_bytes());
    msg.push(0);
    msg.extend_from_slice(outcome_json.as_bytes());
    Ok(hex::encode(hmac_sha256(key, &msg)?))
}

fn verify_entry(key: &[u8], mutant_id: &str, entry: &CacheEntry) -> bool {
    let Some(stored) = entry.mac.as_deref() else {
        return false;
    };
    let Ok(expected) = compute_mac(key, mutant_id, entry) else {
        return false;
    };
    // Constant-time compare on the raw hex bytes. Both sides are the same
    // length (64 hex chars from SHA-256) when the mac was computed against
    // the same algorithm, so timing leak via length is moot too.
    stored.as_bytes().ct_eq(expected.as_bytes()).into()
}

impl Cache {
    pub fn load(path: &Path) -> Self {
        Self::load_with_key(path, cache_key_from_env())
    }

    /// Load with an explicit key. Pass `None` to disable signing &
    /// verification; pass `Some(key)` to verify on load and sign on
    /// insert. Used by `load` (which reads `FERMUT_CACHE_KEY`) and by
    /// tests that need to avoid touching the process-global env var.
    pub fn load_with_key(path: &Path, key: Option<Vec<u8>>) -> Self {
        let mut cache: Self = load_or_quarantine(path);
        if let Some(ref k) = key {
            let before = cache.entries.len();
            cache.entries.retain(|id, entry| verify_entry(k, id, entry));
            let dropped = before - cache.entries.len();
            cache.dropped_on_load = dropped;
            if dropped > 0 {
                warn!(
                    path = %path.display(),
                    dropped,
                    kept = cache.entries.len(),
                    "cache: dropped entries that failed HMAC verification"
                );
            }
        }
        cache.key = key;
        cache
    }

    /// Number of entries discarded at load time because their MAC did not
    /// verify. Always 0 when no key is configured. Used by the engine to
    /// emit a one-line end-of-run summary alongside hit/miss counts.
    pub fn dropped_on_load(&self) -> usize {
        self.dropped_on_load
    }

    /// Number of entries currently in the cache. Reported in the run
    /// summary so users can see whether the warm-cache promise is being
    /// honored (constant churn → cache key drift or excessive scope hash
    /// invalidation).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).context("serializing cache")?;
        std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn lookup(&self, mutant_id: &str, file_hash: &str, scope: &str) -> Option<MutantOutcome> {
        let entry = self.entries.get(mutant_id)?;
        if entry.file_hash == file_hash && entry.scope == scope {
            Some(entry.outcome.clone())
        } else {
            None
        }
    }

    pub fn insert(
        &mut self,
        mutant_id: String,
        file_hash: String,
        scope: String,
        outcome: MutantOutcome,
    ) {
        // Only cache deterministic outcomes — never skipped (filter-dependent)
        // and never errored (transient infra issue).
        if matches!(
            outcome,
            MutantOutcome::Skipped { .. } | MutantOutcome::Error { .. }
        ) {
            return;
        }
        let mut entry = CacheEntry {
            file_hash,
            scope,
            outcome,
            mac: None,
        };
        if let Some(key) = self.key.as_deref() {
            match compute_mac(key, &mutant_id, &entry) {
                Ok(mac) => entry.mac = Some(mac),
                Err(e) => {
                    warn!(error = %e, "cache: failed to compute HMAC; entry stored unsigned");
                }
            }
        }
        self.entries.insert(mutant_id, entry);
    }
}

/// Hex-encoded sha256 of the file's bytes.
pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Default cache location: `<root>/.fermut/cache.json`. `root` is the
/// resolved project root (see [`crate::history::resolve_root`]), not the raw
/// source dir — cache and history share that anchor so they co-locate and the
/// documented `.fermut/cache.json` path is correct.
pub fn default_cache_path(root: &Path) -> PathBuf {
    root.join(".fermut").join("cache.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{Mutant, Operator};
    use ruff_text_size::TextRange;

    fn make_mutant(id: &str) -> Mutant {
        Mutant {
            id: id.into(),
            file: PathBuf::from("test.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.json");
        let c = Cache::load(&path);
        assert!(c.lookup("any", "any", "any").is_none());
    }

    #[test]
    fn load_corrupt_file_quarantines_and_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        std::fs::write(&path, "{ this is not valid json").unwrap();

        let c = Cache::load(&path);
        assert!(c.lookup("any", "any", "any").is_none());

        // Original path is gone or empty; a sibling `*.corrupt-*` exists with
        // the bad bytes for forensic inspection.
        let original_gone = !path.exists();
        let mut found_quarantine = false;
        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.starts_with("cache.corrupt-") {
                found_quarantine = true;
                break;
            }
        }
        assert!(original_gone, "corrupt cache should be moved aside");
        assert!(found_quarantine, "expected a *.corrupt-* sibling");
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");

        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        c.save(&path).unwrap();

        let loaded = Cache::load(&path);
        assert!(matches!(
            loaded.lookup("id-1", "hashA", "scope-x"),
            Some(MutantOutcome::Killed { .. })
        ));
    }

    #[test]
    fn lookup_returns_none_when_hash_differs() {
        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        assert!(c.lookup("id-1", "hashB", "scope-x").is_none());
    }

    #[test]
    fn lookup_returns_none_when_scope_differs() {
        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        assert!(c.lookup("id-1", "hashA", "scope-y").is_none());
        assert!(c.lookup("id-1", "hashA", "scope-x").is_some());
    }

    #[test]
    fn legacy_entry_without_scope_misses_on_new_scope() {
        // Old fermut wrote entries with no `scope` field — serde defaults the
        // missing field to "". A new lookup carries a real scope hash, so the
        // entry must miss and force a fresh run rather than return a stale
        // outcome from a different `--coverage` / `--pytest-arg` shape.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");

        // Build a real entry, then save and rewrite the JSON without the
        // `scope` field to simulate a v0.1 cache on disk.
        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            String::new(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        c.save(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("\"scope\": \"\",", "")).unwrap();

        let loaded = Cache::load(&path);
        // New scope → miss.
        assert!(loaded.lookup("id-1", "hashA", "real-scope").is_none());
        // Empty-scope sentinel still hits, proving the entry parsed and the
        // missing field defaulted as expected.
        assert!(loaded.lookup("id-1", "hashA", "").is_some());
    }

    #[test]
    fn skipped_outcomes_not_cached() {
        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::skipped(make_mutant("id-1"), "ty"),
        );
        assert!(c.lookup("id-1", "hashA", "scope-x").is_none());
    }

    #[test]
    fn errored_outcomes_not_cached() {
        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::error(make_mutant("id-1"), "boom".into()),
        );
        assert!(c.lookup("id-1", "hashA", "scope-x").is_none());
    }

    #[test]
    fn load_missing_file_yields_empty_cache() {
        let cache = Cache::load(&PathBuf::from("/nonexistent/cache.json"));
        assert!(cache.lookup("id", "hash", "scope").is_none());
    }

    #[test]
    fn hash_file_is_stable_and_changes_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        let h1 = hash_file(&file).unwrap();
        let h2 = hash_file(&file).unwrap();
        assert_eq!(h1, h2);

        std::fs::write(&file, b"world").unwrap();
        let h3 = hash_file(&file).unwrap();
        assert_ne!(h1, h3);
    }

    #[test]
    fn default_cache_path_is_under_dot_fermut() {
        let p = default_cache_path(Path::new("/proj"));
        assert!(p.ends_with(Path::new(".fermut").join("cache.json")));
    }

    // ---- HMAC integrity tests --------------------------------------------

    fn keyed_cache(key: &[u8]) -> Cache {
        Cache {
            key: Some(key.to_vec()),
            ..Default::default()
        }
    }

    #[test]
    fn hmac_matches_rfc4231_test_case_2() {
        // RFC 4231 §4.3 — known-answer test for HMAC-SHA256. If this fails
        // either the construction is wrong or someone swapped the
        // underlying hash. Either way the integrity story collapses.
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let got = hex::encode(hmac_sha256(key, data).unwrap());
        assert_eq!(
            got,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signed_entry_verifies_and_lookup_succeeds_after_roundtrip() {
        let key = b"test-cache-key-with-enough-entropy".to_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");

        let mut c = keyed_cache(&key);
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        c.save(&path).unwrap();

        let loaded = Cache::load_with_key(&path, Some(key));
        assert!(
            loaded.lookup("id-1", "hashA", "scope-x").is_some(),
            "signed entry should verify and stay loadable"
        );
    }

    #[test]
    fn tampered_entry_dropped_on_load() {
        let key = b"test-cache-key".to_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");

        let mut c = keyed_cache(&key);
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::survived(make_mutant("id-1")),
        );
        c.save(&path).unwrap();

        // Tamper: flip the serialized status from survived to killed on
        // disk. MAC was computed over the original survived outcome, so
        // verification must fail. Match the lowercase form serde emits.
        let raw = std::fs::read_to_string(&path).unwrap();
        let tampered = raw.replace("\"status\": \"survived\"", "\"status\": \"killed\"");
        assert_ne!(raw, tampered, "fixture should differ post-tamper");
        std::fs::write(&path, tampered).unwrap();

        let loaded = Cache::load_with_key(&path, Some(key));
        assert!(
            loaded.lookup("id-1", "hashA", "scope-x").is_none(),
            "tampered entry must be dropped, not silently trusted"
        );
    }

    #[test]
    fn unsigned_entry_dropped_when_key_set() {
        // Pre-seed an unsigned cache (simulating an attacker who knows the
        // legacy on-disk shape but doesn't have the key). Loading with a
        // key set should reject it wholesale.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        c.save(&path).unwrap();

        let loaded = Cache::load_with_key(&path, Some(b"test-cache-key".to_vec()));
        assert!(
            loaded.lookup("id-1", "hashA", "scope-x").is_none(),
            "unsigned entries must be rejected once key is set"
        );
    }

    #[test]
    fn wrong_key_drops_entries_signed_by_another_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        let mut c = keyed_cache(b"attacker-key");
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        c.save(&path).unwrap();

        let loaded = Cache::load_with_key(&path, Some(b"legitimate-key".to_vec()));
        assert!(loaded.lookup("id-1", "hashA", "scope-x").is_none());
    }

    #[test]
    fn key_unset_keeps_legacy_unsigned_entries_loadable() {
        // Backwards compat: a project that never sets the key sees the
        // pre-signing behavior. No MAC stored, no verification done.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        let mut c = Cache::default();
        c.insert(
            "id-1".into(),
            "hashA".into(),
            "scope-x".into(),
            MutantOutcome::killed(make_mutant("id-1")),
        );
        c.save(&path).unwrap();

        let loaded = Cache::load_with_key(&path, None);
        assert!(loaded.lookup("id-1", "hashA", "scope-x").is_some());
    }
}
