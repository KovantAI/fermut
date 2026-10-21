//! Content fingerprints of the test-suite tree, for the verdict cache.
//!
//! A mutant's outcome depends on the tests as much as on its source: editing a
//! test (without touching the source AST) can turn a survivor into a kill. The
//! cache key therefore folds in a fingerprint of the tests — but only of the
//! tests that can actually run against the mutant, so an edit to an unrelated
//! test doesn't flush every cached verdict.
//!
//! The tree is walked and hashed once per run ([`TestTree::walk`]); each
//! mutant's fingerprint is then folded from that map, so no file is re-hashed
//! per mutant. Two fingerprints are available:
//!
//! - [`TestTree::whole`] — the whole tree. Used when the set of tests a mutant
//!   runs isn't known precisely (no coverage, `unittest`, no recorded context,
//!   a covering file outside the tree).
//! - [`TestTree::scoped`] — a given set of test files plus everything pytest
//!   loads alongside them: the `conftest.py` chain from the tests root down to
//!   each file's directory, and every shared support file (anything under the
//!   tree that is neither a test module nor a `conftest.py` — helpers,
//!   `__init__.py`, fixture data). Support files are included conservatively:
//!   any change to one invalidates every scoped fingerprint.
//!
//! Residual risk, shared with the whole-tree fingerprint: a test that imports
//! a helper outside both the tests tree and the mutated source, or a
//! `conftest.py` above the tests root, is not tracked. A test module that
//! imports another *test module* (`from tests.test_a import helper`) is also
//! not tracked by the scoped fingerprint — move such helpers into a support
//! file.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Hashed snapshot of the test-suite tree rooted at `root`.
#[derive(Debug, Default)]
pub(crate) struct TestTree {
    /// Canonical tests root (as given when canonicalization fails). File keys
    /// in `files` are relative to it.
    root: PathBuf,
    /// Relative path → content hash, for every file the walk could read.
    files: BTreeMap<PathBuf, [u8; 32]>,
    /// Whole-tree fingerprint, hex.
    whole: String,
    /// Digest over every shared support file (see module docs).
    support: [u8; 32],
}

impl TestTree {
    /// Walk and hash the tree at `tests_path`.
    ///
    /// Walks the tree the same way the worker mirror does — `.gitignore`
    /// honored, hidden directories and `__pycache__` pruned, compiled bytecode
    /// skipped. A missing or empty tree hashes to a constant (the empty
    /// digest), which is fine: it still differs from any populated tree.
    pub(crate) fn walk(tests_path: &Path) -> Self {
        let root = tests_path
            .canonicalize()
            .unwrap_or_else(|_| tests_path.to_path_buf());
        let mut files: BTreeMap<PathBuf, [u8; 32]> = BTreeMap::new();
        let walker = ignore::WalkBuilder::new(&root)
            .hidden(false)
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
            let path = entry.path();
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            // Stale compiled bytecode outside `__pycache__` never affects the run.
            if path.extension().is_some_and(|e| e == "pyc" || e == "pyo") {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else {
                // Unreadable file: skip it rather than abort. Its absence from
                // the whole-tree fingerprint is conservative — at worst a cache
                // entry lives one run too long, never the inverse. A covering
                // file missing from `files` sends its mutant to the whole-tree
                // fallback (see `scoped`).
                continue;
            };
            let rel = path.strip_prefix(&root).unwrap_or(path).to_path_buf();
            files.insert(rel, Sha256::digest(&bytes).into());
        }

        let mut whole = Sha256::new();
        let mut support = Sha256::new();
        for (rel, fh) in &files {
            fold_file(&mut whole, rel, fh);
            if !is_test_module(rel) && !is_conftest(rel) {
                fold_file(&mut support, rel, fh);
            }
        }
        Self {
            root,
            files,
            whole: hex::encode(whole.finalize()),
            support: support.finalize().into(),
        }
    }

    /// Whole-tree fingerprint, hex.
    pub(crate) fn whole(&self) -> &str {
        &self.whole
    }

    /// The tracked tree-relative path of `abs`, or `None` when it lies outside
    /// the tree or the walk didn't hash it (ignored, unreadable, missing).
    pub(crate) fn relative<'a>(&self, abs: &'a Path) -> Option<&'a Path> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        self.files.contains_key(rel).then_some(rel)
    }

    /// Fingerprint of `test_files` (tree-relative, as returned by
    /// [`relative`](Self::relative)) plus their `conftest.py` chains and the
    /// shared support files. Hex. Callers must only pass tracked paths; an
    /// untracked one contributes nothing.
    pub(crate) fn scoped<'a>(&self, test_files: impl IntoIterator<Item = &'a Path>) -> String {
        let mut included: BTreeSet<&Path> = BTreeSet::new();
        for rel in test_files {
            included.insert(rel);
            // Every `conftest.py` from the tests root down to the file's own
            // directory. Siblings' conftests are not on the chain.
            for dir in rel.ancestors().skip(1) {
                let candidate = dir.join("conftest.py");
                if let Some((key, _)) = self.files.get_key_value(candidate.as_path()) {
                    included.insert(key.as_path());
                }
            }
        }
        let mut h = Sha256::new();
        h.update(b"support\0");
        h.update(self.support);
        for rel in included {
            if let Some(fh) = self.files.get(rel) {
                fold_file(&mut h, rel, fh);
            }
        }
        hex::encode(h.finalize())
    }
}

fn fold_file(h: &mut Sha256, rel: &Path, fh: &[u8; 32]) {
    h.update(rel.to_string_lossy().as_bytes());
    h.update(b"\0");
    h.update(fh);
}

/// Pytest's default test-module patterns: `test_*.py` and `*_test.py`. A
/// project with a custom `python_files` gets its modules classed as support
/// files — over-invalidation only, never a stale hit.
fn is_test_module(rel: &Path) -> bool {
    rel.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| (n.starts_with("test_") && n.ends_with(".py")) || n.ends_with("_test.py"))
}

fn is_conftest(rel: &Path) -> bool {
    rel.file_name().is_some_and(|n| n == "conftest.py")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// tests/
    ///   conftest.py
    ///   helpers.py
    ///   a/conftest.py
    ///   a/test_a.py
    ///   b/conftest.py
    ///   b/test_b.py
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        write(t, "conftest.py", "root = 1\n");
        write(t, "helpers.py", "def h(): pass\n");
        write(t, "a/conftest.py", "a = 1\n");
        write(t, "a/test_a.py", "def test_a(): pass\n");
        write(t, "b/conftest.py", "b = 1\n");
        write(t, "b/test_b.py", "def test_b(): pass\n");
        tmp
    }

    fn scoped_a(root: &Path) -> String {
        TestTree::walk(root).scoped([Path::new("a/test_a.py")])
    }

    #[test]
    fn whole_is_stable_and_tracks_content() {
        let tmp = fixture();
        let h1 = TestTree::walk(tmp.path()).whole().to_string();
        assert_eq!(
            h1,
            TestTree::walk(tmp.path()).whole(),
            "must be deterministic"
        );
        write(tmp.path(), "b/test_b.py", "def test_b(): assert 1\n");
        assert_ne!(
            h1,
            TestTree::walk(tmp.path()).whole(),
            "any edit changes it"
        );
    }

    #[test]
    fn whole_ignores_pycache_and_bytecode() {
        let tmp = fixture();
        let baseline = TestTree::walk(tmp.path()).whole().to_string();
        write(tmp.path(), "a/__pycache__/test_a.cpython-312.pyc", "junk");
        write(tmp.path(), "a/stale.pyc", "junk");
        assert_eq!(baseline, TestTree::walk(tmp.path()).whole());
    }

    #[test]
    fn scoped_is_stable() {
        let tmp = fixture();
        assert_eq!(scoped_a(tmp.path()), scoped_a(tmp.path()));
    }

    #[test]
    fn scoped_ignores_unrelated_test_and_sibling_conftest() {
        let tmp = fixture();
        let before = scoped_a(tmp.path());
        write(tmp.path(), "b/test_b.py", "def test_b(): assert 1\n");
        write(tmp.path(), "b/conftest.py", "b = 2\n");
        write(tmp.path(), "b/test_new.py", "def test_new(): pass\n");
        assert_eq!(before, scoped_a(tmp.path()));
    }

    #[test]
    fn scoped_tracks_the_covering_file() {
        let tmp = fixture();
        let before = scoped_a(tmp.path());
        write(tmp.path(), "a/test_a.py", "def test_a(): assert 1\n");
        assert_ne!(before, scoped_a(tmp.path()));
    }

    #[test]
    fn scoped_tracks_the_conftest_chain() {
        for rel in ["conftest.py", "a/conftest.py"] {
            let tmp = fixture();
            let before = scoped_a(tmp.path());
            write(tmp.path(), rel, "changed = 1\n");
            assert_ne!(before, scoped_a(tmp.path()), "{rel} is on the chain");
        }
    }

    #[test]
    fn scoped_tracks_a_new_conftest_on_the_chain() {
        let tmp = fixture();
        write(tmp.path(), "a/deep/test_d.py", "def test_d(): pass\n");
        let fp = |root: &Path| TestTree::walk(root).scoped([Path::new("a/deep/test_d.py")]);
        let before = fp(tmp.path());
        write(tmp.path(), "a/deep/conftest.py", "new = 1\n");
        assert_ne!(before, fp(tmp.path()));
    }

    #[test]
    fn scoped_tracks_support_files() {
        for (rel, body) in [
            ("helpers.py", "def h(): return 1\n"),
            ("b/__init__.py", ""),
            ("b/data/fixture.json", "{}"),
        ] {
            let tmp = fixture();
            let before = scoped_a(tmp.path());
            write(tmp.path(), rel, body);
            assert_ne!(before, scoped_a(tmp.path()), "{rel} is a support file");
        }
    }

    #[test]
    fn relative_rejects_outside_and_untracked_paths() {
        let tmp = fixture();
        let tree = TestTree::walk(tmp.path());
        let root = tmp.path().canonicalize().unwrap();
        assert_eq!(
            tree.relative(&root.join("a/test_a.py")),
            Some(Path::new("a/test_a.py"))
        );
        assert_eq!(tree.relative(&root.join("a/test_missing.py")), None);
        assert_eq!(tree.relative(&root.parent().unwrap().join("x.py")), None);
    }

    #[test]
    fn test_module_patterns() {
        assert!(is_test_module(Path::new("a/test_x.py")));
        assert!(is_test_module(Path::new("x_test.py")));
        assert!(!is_test_module(Path::new("testing.py")));
        assert!(!is_test_module(Path::new("test_data.json")));
        assert!(!is_test_module(Path::new("conftest.py")));
    }
}
