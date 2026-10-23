//! `--select-tests`: narrow coverage-driven test selection to an allowlist of
//! pytest node ids — the tests a `pytest --fermut` session actually selected
//! (`-k`, `-m`, paths, `--deselect`, ...).
//!
//! The run keeps two views of the contexts. The **full** one still decides
//! "no test executes this line" (`coverage` skip). The **narrowed** one, from
//! [`CoverageContexts::select`], is what every other reader sees: per-mutant
//! test selection, smart ordering, the verdict-cache scope and killer lookup.
//! A mutant some test covers but no *selected* test does is skipped by
//! [`SelectionFilter`] under [`SELECTION_FILTER`], never run — so pytest is
//! never handed an empty selection (exit 5), and a cached kill by a test
//! outside the selection can't be reused (`compute_killer_scope` only accepts
//! a killer the narrowed contexts list for the mutant).

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::CoverageContexts;
use crate::filter::Filter;
use crate::mutator::Mutant;

/// Filter name recorded on mutants covered only by tests outside the
/// `--select-tests` allowlist. Callers that treat a `coverage` skip as
/// "deliberately not run" (e.g. `fermut next`) treat this one the same.
pub const SELECTION_FILTER: &str = "selection";

/// Read a `--select-tests` file: one pytest node id per line, either
/// `<absolute test file>::<rest>` or already relative to `node_root` (the
/// directory the coverage node ids are relative to). Absolute paths let the
/// caller (the pytest plugin) ignore which directory fermut picked as project
/// root — pytest's rootdir can differ (`pytest.ini`, `tox.ini`, `--rootdir`).
/// Blank lines are ignored; an entry outside `node_root` is dropped with a
/// warning since no coverage context can name it.
pub fn read_selection(path: &Path, node_root: &Path) -> Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading --select-tests file {}", path.display()))?;
    // Canonicalize each test file once: node ids repeat their file per test.
    let mut rebased: HashMap<&str, Option<String>> = HashMap::new();
    let mut selected = BTreeSet::new();
    let mut outside = 0usize;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (file, rest) = line
            .split_once("::")
            .map_or((line, None), |(f, r)| (f, Some(r)));
        let rel = rebased
            .entry(file)
            .or_insert_with(|| relative_node_path(Path::new(file), node_root));
        match rel {
            Some(rel) => {
                selected.insert(match rest {
                    Some(r) => format!("{rel}::{r}"),
                    None => rel.clone(),
                });
            }
            None => outside += 1,
        }
    }
    if outside > 0 {
        warn!(
            count = outside,
            node_root = %node_root.display(),
            "dropped selected tests outside the coverage node root"
        );
    }
    Ok(selected)
}

/// `file` as a `/`-separated node-id path relative to `node_root`, or `None`
/// when an absolute `file` lies outside it. A relative `file` is taken as
/// already relative to `node_root`.
///
/// Paths are resolved the way pytest builds node ids: `..` lexically, and
/// symlinks below the root (a linked test file *or* directory) never, since
/// pytest names a test by its link. Only an alias of the root itself
/// (`/var` → `/private/var`) is resolved, by finding the ancestor that
/// canonicalizes to `node_root`.
fn relative_node_path(file: &Path, node_root: &Path) -> Option<String> {
    let rel = if file.is_absolute() {
        let mut lexical = std::path::PathBuf::new();
        for c in file.components() {
            match c {
                Component::ParentDir => {
                    lexical.pop();
                }
                Component::CurDir => {}
                c => lexical.push(c),
            }
        }
        match lexical.strip_prefix(node_root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => {
                let root = lexical
                    .ancestors()
                    .find(|a| a.canonicalize().is_ok_and(|c| c == node_root))?;
                lexical.strip_prefix(root).ok()?.to_path_buf()
            }
        }
    } else {
        file.to_path_buf()
    };
    let parts: Vec<_> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

impl CoverageContexts {
    /// A copy restricted to the `selected` node ids. Test ids are kept when
    /// selected. A module id (an import-time `|collect` context, no `::`) is
    /// **replaced** by the selected tests in that module: the runner hands a
    /// module id to pytest as a path, which would run the module's
    /// deselected tests too. Lines left with no test are dropped, so
    /// [`tests_for_mutant`](Self::tests_for_mutant) returns `None` for them.
    pub fn select(&self, selected: &BTreeSet<String>) -> Arc<Self> {
        let map = self
            .map
            .iter()
            .filter_map(|(path, per_line)| {
                let per_line: HashMap<u32, Vec<String>> = per_line
                    .iter()
                    .filter_map(|(&line, ids)| {
                        let kept: BTreeSet<&String> = ids
                            .iter()
                            .flat_map(|id| selected_for(id, selected))
                            .collect();
                        (!kept.is_empty()).then(|| (line, kept.into_iter().cloned().collect()))
                    })
                    .collect();
                (!per_line.is_empty()).then(|| (path.clone(), per_line))
            })
            .collect();
        Arc::new(Self::from_map(map, self.node_root.clone()))
    }
}

/// The selected ids a coverage context id stands for: itself when it's a
/// selected test id, the selected tests of the module when it's a module id.
fn selected_for<'a>(id: &str, selected: &'a BTreeSet<String>) -> Vec<&'a String> {
    if id.contains("::") {
        return selected.get(id).into_iter().collect();
    }
    let prefix = format!("{id}::");
    selected
        .range(prefix.clone()..)
        .take_while(|s| s.starts_with(&prefix))
        .collect()
}

/// Skips mutants no selected test covers. Chained right after
/// [`super::CoverageFilter`] (which runs on the full contexts), so it only
/// sees covered mutants and its skips mean "outside the selection".
///
/// It checks the line the *full* contexts matched — the mutant's own line,
/// else its statement's first line — not `narrowed.tests_for_mutant`:
/// narrowing drops lines, and the statement-line fallback would then admit a
/// mutant on a line only unselected tests run, via selected tests that never
/// reach it (a false survivor).
pub struct SelectionFilter {
    full: Arc<CoverageContexts>,
    narrowed: Arc<CoverageContexts>,
}

impl SelectionFilter {
    pub fn new(full: Arc<CoverageContexts>, narrowed: Arc<CoverageContexts>) -> Self {
        Self { full, narrowed }
    }
}

impl Filter for SelectionFilter {
    fn name(&self) -> &'static str {
        SELECTION_FILTER
    }

    fn admits(&self, m: &Mutant) -> Result<bool> {
        let covered =
            |c: &CoverageContexts, line| c.tests_for(&m.file, line).is_some_and(|t| !t.is_empty());
        let line = if covered(&self.full, m.line) {
            m.line
        } else {
            m.stmt_line
        };
        let admitted = line != 0 && covered(&self.narrowed, line);
        if !admitted {
            debug!(
                file = %m.file.display(),
                line = m.line,
                "no selected test covers mutant; skipping"
            );
        }
        Ok(admitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(lines: &[(u32, &[&str])]) -> CoverageContexts {
        let per_line = lines
            .iter()
            .map(|(l, ids)| (*l, ids.iter().map(|s| s.to_string()).collect()))
            .collect();
        CoverageContexts::from_map(
            HashMap::from([(PathBuf::from("/p/src/m.py"), per_line)]),
            PathBuf::from("/p"),
        )
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn ids(c: &CoverageContexts, line: u32) -> Option<Vec<&str>> {
        c.tests_for(Path::new("/p/src/m.py"), line)
            .map(|v| v.iter().map(String::as_str).collect())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_test_file_keeps_its_link_name() {
        // pytest names a symlinked test by the link, so must the selection.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("shared/test_x.py"), "").unwrap();
        std::os::unix::fs::symlink("../shared/test_x.py", root.join("tests/test_link.py")).unwrap();
        let link = root.join("tests/test_link.py");
        assert_eq!(
            relative_node_path(&link, &root).as_deref(),
            Some("tests/test_link.py")
        );
        // A symlinked root (`/var` → `/private/var`) still resolves.
        let alias = tmp.path().parent().unwrap().join(format!(
            "{}-alias",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let got = relative_node_path(&alias.join("tests/test_link.py"), &root);
        let got_dir = relative_node_path(&alias.join("tests/link/test_y.py"), &root);
        std::fs::remove_file(&alias).unwrap();
        assert_eq!(got.as_deref(), Some("tests/test_link.py"));
        assert_eq!(got_dir.as_deref(), Some("tests/link/test_y.py"));
        // Symlinked test directories keep their link name, whether the link
        // points inside the root or outside it.
        std::fs::create_dir_all(root.join("tests/real")).unwrap();
        std::os::unix::fs::symlink("real", root.join("tests/link")).unwrap();
        assert_eq!(
            relative_node_path(&root.join("tests/link/test_y.py"), &root).as_deref(),
            Some("tests/link/test_y.py")
        );
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("tests/integration")).unwrap();
        assert_eq!(
            relative_node_path(&root.join("tests/integration/test_z.py"), &root).as_deref(),
            Some("tests/integration/test_z.py")
        );
        // A test outside pytest's rootdir: `<rootdir>/../tests/...`.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        assert_eq!(
            relative_node_path(&root.join("sub/../tests/test_link.py"), &root).as_deref(),
            Some("tests/test_link.py")
        );
    }

    #[test]
    fn keeps_exact_and_parametrized_ids_drops_unselected() {
        let full = ctx(&[(1, &["t/a.py::x", "t/a.py::y[1-2]", "t/b.py::z"])]);
        let n = full.select(&set(&["t/a.py::y[1-2]", "t/b.py::z"]));
        assert_eq!(ids(&n, 1), Some(vec!["t/a.py::y[1-2]", "t/b.py::z"]));
    }

    #[test]
    fn line_with_no_selected_test_is_dropped() {
        let full = ctx(&[(1, &["t/a.py::x"]), (2, &["t/b.py::z"])]);
        let n = full.select(&set(&["t/b.py::z"]));
        assert_eq!(ids(&n, 1), None);
        assert_eq!(ids(&n, 2), Some(vec!["t/b.py::z"]));
        // The full view is untouched: line 1 stays covered there.
        assert_eq!(ids(&full, 1), Some(vec!["t/a.py::x"]));
    }

    #[test]
    fn module_id_expands_to_selected_tests_of_that_module_only() {
        // `t/a.py` is an import-time (`|collect`) context. Passing it through
        // would make pytest run all of t/a.py, deselected tests included.
        let full = ctx(&[(1, &["t/a.py"])]);
        let n = full.select(&set(&["t/a.py::x", "t/a.pyz::w", "t/b.py::z"]));
        assert_eq!(ids(&n, 1), Some(vec!["t/a.py::x"]));
    }

    #[test]
    fn module_id_with_no_selected_test_drops_line() {
        let full = ctx(&[(1, &["t/a.py"])]);
        assert_eq!(ids(&full.select(&set(&["t/b.py::z"])), 1), None);
    }

    #[test]
    fn selection_filter_skips_covered_but_unselected() {
        let full = ctx(&[(1, &["t/a.py::x"]), (2, &["t/b.py::z"])]);
        let full = Arc::new(full);
        let f = SelectionFilter::new(full.clone(), full.select(&set(&["t/b.py::z"])));
        let at = |line| super::super::tests::make_mutant(PathBuf::from("/p/src/m.py"), line);
        assert!(!f.admits(&at(1)).unwrap());
        assert!(f.admits(&at(2)).unwrap());
        assert_eq!(f.name(), "selection");
    }

    #[test]
    fn selection_filter_ignores_statement_line_of_a_dropped_line() {
        // `sorted(items,\n    key=lambda v: v.x)`: line 2 (the lambda) runs
        // only under unselected t/a.py::x; line 1 under selected t/b.py::z.
        let full = Arc::new(ctx(&[
            (1, &["t/a.py::x", "t/b.py::z"]),
            (2, &["t/a.py::x"]),
        ]));
        let f = SelectionFilter::new(full.clone(), full.select(&set(&["t/b.py::z"])));
        let mut m = super::super::tests::make_mutant(PathBuf::from("/p/src/m.py"), 2);
        m.stmt_line = 1;
        assert!(!f.admits(&m).unwrap(), "t/b.py::z never runs line 2");
        // A line the full contexts don't cover still falls back to its statement.
        let mut m = super::super::tests::make_mutant(PathBuf::from("/p/src/m.py"), 3);
        m.stmt_line = 1;
        assert!(f.admits(&m).unwrap());
    }

    #[test]
    fn read_selection_rebases_absolute_and_keeps_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        let other = tempfile::tempdir().unwrap();
        let list = root.join("sel.txt");
        std::fs::write(
            &list,
            format!(
                "{abs}::TestA::test_x[a::b]\n\n  tests/test_r.py::test_y  \n{out}::test_z\n",
                abs = root.join("tests/test_a.py").display(),
                out = other.path().join("test_o.py").display(),
            ),
        )
        .unwrap();
        let sel = read_selection(&list, &root).unwrap();
        assert_eq!(
            sel,
            set(&[
                "tests/test_a.py::TestA::test_x[a::b]",
                "tests/test_r.py::test_y"
            ])
        );
    }
}
