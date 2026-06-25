//! Persistent cache for LLM responses.
//!
//! Mirrors the contract of the result cache: keyed by a content hash so any
//! change to mutant id, source file bytes, or prompt invalidates the entry.
//! Default location `.fermut/llm-cache.json`. Atomic write on save.
//!
//! LLM calls cost money and time. Re-running `fermut suggest` on the same
//! survivor with the same source should never re-hit the API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache::load_or_quarantine;

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct LlmCache {
    #[serde(default)]
    entries: HashMap<String, String>,
}

impl LlmCache {
    pub fn load(path: &Path) -> Self {
        load_or_quarantine(path)
    }

    /// Atomic save: write to a sibling tmp file, then rename onto `path`.
    /// A crash between write and rename leaves the previous cache intact —
    /// `std::fs::write` would have truncated first, so an interrupted save
    /// could leave a half-written / empty file that `load` then parses as
    /// "no entries", silently re-billing every cached prompt on the next
    /// run. The tmp name includes the PID so concurrent invocations against
    /// the same cache don't clobber each other's in-flight writes.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).context("serializing llm cache")?;
        let tmp = tmp_sibling(path);
        std::fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            // Best-effort cleanup so a failed rename doesn't leave debris.
            let _ = std::fs::remove_file(&tmp);
            return Err(e)
                .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()));
        }
        Ok(())
    }

    pub fn lookup(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn insert(&mut self, key: String, response: String) {
        self.entries.insert(key, response);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Default LLM cache path under a project root. Lives alongside the
/// existing result cache under `.fermut/`.
pub fn default_cache_path(project_root: &Path) -> PathBuf {
    project_root.join(".fermut").join("llm-cache.json")
}

/// Build the tmp sibling name for an atomic save. Kept in the same parent
/// directory as `path` so `rename` stays on one filesystem (POSIX rename
/// across mount points fails with `EXDEV`). PID-scoped suffix avoids
/// collisions when multiple `fermut` processes race on the same cache.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("cache"));
    name.push(format!(".tmp.{}", std::process::id()));
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".fermut").join("llm-cache.json");
        let mut c = LlmCache::default();
        c.insert("key1".into(), "hello".into());
        c.insert("key2".into(), "world".into());
        c.save(&path).unwrap();

        let loaded = LlmCache::load(&path);
        assert_eq!(loaded.lookup("key1"), Some("hello"));
        assert_eq!(loaded.lookup("key2"), Some("world"));
        assert_eq!(loaded.lookup("missing"), None);
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let c = LlmCache::load(&dir.path().join("nope.json"));
        assert!(c.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "not json").unwrap();
        let c = LlmCache::load(&path);
        assert!(c.is_empty());
    }

    #[test]
    fn load_corrupt_file_quarantines_bad_bytes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{ broken").unwrap();
        let _ = LlmCache::load(&path);
        // Corrupt file moved aside so a subsequent save doesn't silently
        // overwrite the only record of what was on disk.
        assert!(!path.exists());
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("c.corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "expected one quarantine file");
    }

    #[test]
    fn default_cache_path_sits_under_dot_fermut() {
        let p = default_cache_path(Path::new("/tmp/proj"));
        assert!(p.ends_with(".fermut/llm-cache.json"));
    }

    #[test]
    fn tmp_sibling_lives_next_to_target() {
        let p = Path::new("/tmp/proj/.fermut/llm-cache.json");
        let tmp = tmp_sibling(p);
        assert_eq!(tmp.parent(), p.parent());
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("llm-cache.json.tmp."),
            "unexpected tmp name `{name}`"
        );
    }

    /// Regression: a crash between `truncate` and `write` in `std::fs::write`
    /// used to leave an empty file that `load` parses as zero entries. The
    /// atomic rename keeps the previous cache intact when the tmp write is
    /// interrupted. We simulate the partial-write half by leaving a stray
    /// tmp file behind and asserting `load` still sees the prior contents.
    #[test]
    fn load_recovers_when_partial_tmp_file_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".fermut").join("llm-cache.json");
        let mut c = LlmCache::default();
        c.insert("key1".into(), "hello".into());
        c.save(&path).unwrap();

        // Simulate an interrupted save: a half-written tmp file present,
        // but the rename never completed.
        let tmp = path.with_file_name("llm-cache.json.tmp.99999");
        std::fs::write(&tmp, "{").unwrap();

        let loaded = LlmCache::load(&path);
        assert_eq!(loaded.lookup("key1"), Some("hello"));
    }

    #[test]
    fn save_does_not_leave_tmp_file_behind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".fermut").join("llm-cache.json");
        let mut c = LlmCache::default();
        c.insert("key1".into(), "hello".into());
        c.save(&path).unwrap();

        let entries: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected only the cache file, got {entries:?}"
        );
        assert_eq!(entries[0], path.file_name().unwrap());
    }
}
