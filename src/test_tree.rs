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
//! Both fingerprints also fold in every `conftest.py` above the tests root.
//!
//! `scoped` can narrow a test module to one test (a `test*` function, or a
//! `test*` method of a `Test*` class): the test's own source, the source of
//! every other test it names (a base class, a called test), and every
//! statement that isn't a test (imports, constants, fixtures, helpers; for a
//! method also its class's header, attributes and non-test methods). Other
//! tests don't count, so adding a test next to a killer keeps its kills. An
//! inherited method counts its whole class and the bases it names. A module that doesn't parse, or a test that isn't a
//! top-level def there, falls back to the whole file. Residual risk: another
//! test's decorator runs at import, so one that changes module state or
//! breaks collection isn't tracked.
//!
//! Residual risk, shared with the whole-tree fingerprint: a test that imports
//! a helper outside both the tests tree and the mutated source is not
//! tracked. A test module that
//! imports another *test module* (`from tests.test_a import helper`) is also
//! not tracked by the scoped fingerprint — move such helpers into a support
//! file.

use ruff_python_ast::Stmt;
use ruff_text_size::{Ranged, TextRange};
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
    /// Per-test split of every test module that parsed.
    units: BTreeMap<PathBuf, Units>,
}

/// A test module, or a test class body, split into its tests and the
/// statements they share.
#[derive(Debug)]
struct Units {
    /// Digest over every statement that isn't a test.
    shared: [u8; 32],
    tests: BTreeMap<String, Unit>,
}

/// One test: a function, or a class whose test methods are units too.
#[derive(Debug)]
struct Unit {
    /// Digest of its source (redefinitions folded in order).
    hash: [u8; 32],
    /// The other tests in the same body it names.
    refs: BTreeSet<String>,
    /// A class defined once: digest of its header (decorators, bases) and its
    /// body, split the same way.
    members: Option<([u8; 32], Units)>,
}

impl Units {
    /// `None` when `source` doesn't parse.
    fn parse(source: &str) -> Option<Self> {
        let parsed = ruff_python_parser::parse_module(source).ok()?;
        Some(Self::split(&parsed.syntax().body, source))
    }

    fn split(body: &[Stmt], source: &str) -> Self {
        let mut shared = Sha256::new();
        let mut defs: BTreeMap<&str, Vec<&Stmt>> = BTreeMap::new();
        for stmt in body {
            // pytest's default `python_functions` / `python_classes`; a
            // custom pattern's tests count as shared (over-invalidation only).
            let name = match stmt {
                Stmt::FunctionDef(f) if f.name.starts_with("test") => Some(f.name.as_str()),
                Stmt::ClassDef(c) if c.name.starts_with("Test") => Some(c.name.as_str()),
                _ => None,
            };
            match name {
                Some(n) => defs.entry(n).or_default().push(stmt),
                None => {
                    shared.update(source[stmt.range()].as_bytes());
                    shared.update(b"\0");
                }
            }
        }
        let tests = defs
            .iter()
            .map(|(&name, stmts)| {
                let mut h = Sha256::new();
                let mut refs = BTreeSet::new();
                for stmt in stmts {
                    let text = &source[stmt.range()];
                    h.update(text.as_bytes());
                    h.update(b"\0");
                    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                        if word != name && defs.contains_key(word) {
                            refs.insert(word.to_string());
                        }
                    }
                }
                let members = match stmts.as_slice() {
                    [Stmt::ClassDef(c)] => c.body.first().map(|first| {
                        let header = &source[TextRange::new(c.start(), first.start())];
                        (
                            Sha256::digest(header.as_bytes()).into(),
                            Self::split(&c.body, source),
                        )
                    }),
                    _ => None,
                };
                let unit = Unit {
                    hash: h.finalize().into(),
                    refs,
                    members,
                };
                (name.to_string(), unit)
            })
            .collect();
        Self {
            shared: shared.finalize().into(),
            tests,
        }
    }

    /// Digest of `test` (`test_x`, or `TestA::test_m` for a method), the tests
    /// it names (transitively, whole) and the shared statements. A method that
    /// doesn't resolve (inherited from a base) counts its whole class. `None`
    /// when the first segment isn't a test here.
    fn fingerprint(&self, test: &str) -> Option<[u8; 32]> {
        let (name, member) = match test.split_once("::") {
            Some((n, m)) => (n, Some(m)),
            None => (test, None),
        };
        let mut seen = BTreeSet::new();
        let mut todo = vec![name];
        while let Some(n) = todo.pop() {
            let unit = self.tests.get(n)?;
            if seen.insert(n) {
                todo.extend(unit.refs.iter().map(String::as_str));
            }
        }
        let narrowed = member.and_then(|m| {
            let (header, members) = self.tests[name].members.as_ref()?;
            Some((header, members.fingerprint(m)?))
        });
        let mut h = Sha256::new();
        h.update(b"unit\0");
        h.update(self.shared);
        for n in seen {
            h.update(n.as_bytes());
            h.update(b"\0");
            match narrowed.filter(|_| n == name) {
                Some((header, inner)) => {
                    h.update(b"member\0");
                    h.update(header);
                    h.update(inner);
                }
                None => h.update(self.tests[n].hash),
            }
        }
        Some(h.finalize().into())
    }
}

impl TestTree {
    /// Walk and hash the tree at `tests_path`.
    ///
    /// Walks the tree the same way the worker mirror does — `.gitignore`
    /// honored, junk directories (`util::is_pruned_dir_name`) pruned, compiled bytecode
    /// skipped. A missing or empty tree hashes to a constant (the empty
    /// digest), which is fine: it still differs from any populated tree.
    pub(crate) fn walk(tests_path: &Path) -> Self {
        let root = tests_path
            .canonicalize()
            .unwrap_or_else(|_| tests_path.to_path_buf());
        let mut files: BTreeMap<PathBuf, [u8; 32]> = BTreeMap::new();
        let mut units: BTreeMap<PathBuf, Units> = BTreeMap::new();
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
                !(is_dir && name.is_some_and(crate::util::is_pruned_dir_name))
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
            if is_test_module(&rel) {
                if let Some(u) = std::str::from_utf8(&bytes).ok().and_then(Units::parse) {
                    units.insert(rel.clone(), u);
                }
            }
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
        // A `conftest.py` above the tests root (e.g. a repo-root one with
        // `tests/` as the root) loads for every test, so it's part of both
        // fingerprints. Keyed by depth (`../conftest.py`), not absolute path,
        // so the key survives a checkout at another path. Every ancestor is
        // checked, like `engine::deselection_key`: one pytest ignores (above
        // its rootdir) only over-invalidates.
        let mut up = PathBuf::new();
        for dir in root.ancestors().skip(1) {
            up.push("..");
            if let Ok(bytes) = std::fs::read(dir.join("conftest.py")) {
                let rel = up.join("conftest.py");
                let fh: [u8; 32] = Sha256::digest(&bytes).into();
                fold_file(&mut whole, &rel, &fh);
                fold_file(&mut support, &rel, &fh);
            }
        }
        Self {
            root,
            files,
            whole: hex::encode(whole.finalize()),
            support: support.finalize().into(),
            units,
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
    /// untracked one contributes nothing. A file paired with a test name
    /// counts only that test and the file's shared statements (see module
    /// docs), or the whole file when the name doesn't resolve.
    pub(crate) fn scoped<'a>(
        &self,
        test_files: impl IntoIterator<Item = (&'a Path, Option<&'a str>)>,
    ) -> String {
        let mut included: BTreeSet<&Path> = BTreeSet::new();
        let mut narrowed: BTreeMap<&Path, [u8; 32]> = BTreeMap::new();
        for (rel, test) in test_files {
            match test.and_then(|t| self.units.get(rel)?.fingerprint(t)) {
                Some(fp) => {
                    narrowed.insert(rel, fp);
                }
                None => {
                    included.insert(rel);
                }
            }
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
        for (rel, fp) in narrowed {
            fold_file(&mut h, rel, &fp);
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
        TestTree::walk(root).scoped([(Path::new("a/test_a.py"), None)])
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
    fn whole_tracks_non_junk_hidden_dirs_only() {
        // Same prune as the mirror: a `.data` fixture the tests read is part of
        // the run; `.pytest_cache` is not.
        let tmp = fixture();
        let baseline = TestTree::walk(tmp.path()).whole().to_string();
        write(tmp.path(), ".pytest_cache/v/cache", "junk");
        assert_eq!(baseline, TestTree::walk(tmp.path()).whole());
        write(tmp.path(), ".data/x.json", "{}");
        assert_ne!(baseline, TestTree::walk(tmp.path()).whole());
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
        let fp = |root: &Path| TestTree::walk(root).scoped([(Path::new("a/deep/test_d.py"), None)]);
        let before = fp(tmp.path());
        write(tmp.path(), "a/deep/conftest.py", "new = 1\n");
        assert_ne!(before, fp(tmp.path()));
    }

    #[test]
    fn both_track_a_conftest_above_the_tests_root() {
        // Repo-root `conftest.py` with `tests/` as the tests root (#63).
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "conftest.py", "root = 1\n");
        write(tmp.path(), "tests/a/test_a.py", "def test_a(): pass\n");
        let tests = tmp.path().join("tests");
        let fps = || {
            let tree = TestTree::walk(&tests);
            let scoped = tree.scoped([(Path::new("a/test_a.py"), None)]);
            (tree.whole().to_string(), scoped)
        };
        let (whole, scoped) = fps();
        write(tmp.path(), "conftest.py", "root = 2\n");
        let (whole2, scoped2) = fps();
        assert_ne!(whole, whole2);
        assert_ne!(scoped, scoped2);
        std::fs::remove_file(tmp.path().join("conftest.py")).unwrap();
        assert_ne!(fps(), (whole2, scoped2), "removing it counts too");
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

    const MODULE: &str =
        "import pytest\n\nCASES = [1, 2]\n\n@pytest.fixture\ndef f():\n    return 1\n\n\
        @pytest.mark.parametrize('x', CASES)\ndef test_x(x, f):\n    assert x\n\n\
        def test_y():\n    pass\n\nclass TestA:\n    def test_m(self):\n        pass\n\n\
        class TestB(TestA):\n    pass\n";

    fn unit_fp(module: &str, test: &str) -> String {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "test_m.py", module);
        TestTree::walk(tmp.path()).scoped([(Path::new("test_m.py"), Some(test))])
    }

    #[test]
    fn unit_ignores_other_tests() {
        let before = unit_fp(MODULE, "test_x");
        for edited in [
            format!("{MODULE}\ndef test_new():\n    pass\n"),
            MODULE.replace(
                "def test_y():\n    pass",
                "@pytest.mark.skip\ndef test_y():\n    assert 0",
            ),
            MODULE.replace("import pytest\n", "import pytest\n# comment\n"),
        ] {
            assert_eq!(before, unit_fp(&edited, "test_x"), "{edited}");
        }
    }

    #[test]
    fn unit_tracks_itself_and_shared_statements() {
        let before = unit_fp(MODULE, "test_x");
        for (from, to) in [
            ("    assert x\n", "    assert not x\n"),
            ("parametrize('x', CASES)", "parametrize('x', [3])"),
            ("CASES = [1, 2]", "CASES = [1]"),
            ("    return 1\n", "    return 2\n"),
            ("import pytest\n", "import pytest\nimport os\n"),
        ] {
            assert_ne!(before, unit_fp(&MODULE.replace(from, to), "test_x"), "{to}");
        }
    }

    #[test]
    fn unit_follows_tests_it_names() {
        let edited = MODULE.replace(
            "        pass\n\nclass TestB",
            "        assert 0\n\nclass TestB",
        );
        assert_ne!(unit_fp(MODULE, "TestB"), unit_fp(&edited, "TestB"));
        assert_ne!(unit_fp(MODULE, "TestA"), unit_fp(&edited, "TestA"));
        assert_eq!(unit_fp(MODULE, "test_x"), unit_fp(&edited, "test_x"));
    }

    #[test]
    fn method_unit_ignores_sibling_methods() {
        let before = unit_fp(MODULE, "TestA::test_m");
        let sibling = MODULE.replace(
            "        pass\n\nclass TestB",
            "        pass\n\n    def test_n(self):\n        assert 0\n\nclass TestB",
        );
        assert_eq!(before, unit_fp(&sibling, "TestA::test_m"));
        let setup = MODULE.replace(
            "class TestA:\n",
            "class TestA:\n    def setup_method(self):\n        pass\n\n",
        );
        assert_ne!(before, unit_fp(&setup, "TestA::test_m"), "non-test member");
        let base = MODULE.replace("class TestA:", "class TestA(object):");
        assert_ne!(before, unit_fp(&base, "TestA::test_m"), "class header");
    }

    #[test]
    fn inherited_method_counts_its_bases() {
        let edited = MODULE.replace(
            "        pass\n\nclass TestB",
            "        assert 0\n\nclass TestB",
        );
        assert_ne!(
            unit_fp(MODULE, "TestB::test_m"),
            unit_fp(&edited, "TestB::test_m")
        );
    }

    #[test]
    fn unresolved_unit_falls_back_to_the_file() {
        let file = |module: &str| {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), "test_m.py", module);
            TestTree::walk(tmp.path()).scoped([(Path::new("test_m.py"), None)])
        };
        assert_eq!(unit_fp(MODULE, "test_gone"), file(MODULE));
        let broken = format!("{MODULE}\ndef test_z(:\n");
        assert_eq!(unit_fp(&broken, "test_x"), file(&broken));
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
