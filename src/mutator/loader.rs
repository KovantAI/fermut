//! Tree walk: read every `.py` file under a root and emit mutation candidates.

use std::path::Path;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use super::encoding::{is_utf8_compatible_encoding, strip_bom};
use super::visitor;
use super::Mutant;

/// Walk a Python source tree and return every mutation candidate.
///
/// `exclude` is a list of glob patterns matched against paths relative to
/// `root`. Matching files (and directories — pruned before descent) are
/// skipped, so excluded subtrees never reach the parser.
pub fn collect_from_tree(root: &Path, exclude: &[String]) -> Result<Vec<Mutant>> {
    let excludes = build_globset(exclude)?;
    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_excluded(e.path(), root, &excludes));
    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_python_source(path, root) {
            continue;
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let source = strip_bom(&raw);
        if !is_utf8_compatible_encoding(source) {
            tracing::debug!(
                file = %path.display(),
                "skipping non-utf8 encoding declaration"
            );
            continue;
        }
        let mut mutants = visitor::collect(path, source)?;
        out.append(&mut mutants);
    }
    Ok(out)
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("invalid exclude glob: {p:?}"))?);
    }
    Ok(Some(b.build().context("compiling exclude globs")?))
}

/// Match `path` against the exclude set using its position relative to
/// `root`. Returns false when no globs are configured. The walk root itself
/// is never excluded — otherwise the walker would terminate immediately.
fn is_excluded(path: &Path, root: &Path, excludes: &Option<GlobSet>) -> bool {
    let Some(set) = excludes else {
        return false;
    };
    let rel = match path.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if rel.as_os_str().is_empty() {
        return false;
    }
    set.is_match(rel)
}

fn is_python_source(p: &Path, root: &Path) -> bool {
    if p.extension().and_then(|s| s.to_str()) != Some("py") {
        return false;
    }
    // Skip tests, venvs, and caches by default. Match on path components
    // relative to `root` so this is platform-independent (no hardcoded `/`)
    // and a repo rooted under e.g. `.../tests/...` is not wholly excluded.
    let rel = p.strip_prefix(root).unwrap_or(p);
    !rel.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name == "tests" || name.starts_with("test_") || name == ".venv" || name == "__pycache__"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn is_python_source_skips_tests_and_caches() {
        let root = Path::new("/proj");
        assert!(is_python_source(Path::new("/proj/src/calc.py"), root));
        assert!(!is_python_source(
            Path::new("/proj/tests/test_calc.py"),
            root
        ));
        assert!(!is_python_source(
            Path::new("/proj/src/test_helper.py"),
            root
        ));
        assert!(!is_python_source(Path::new("/proj/.venv/lib/pkg.py"), root));
        assert!(!is_python_source(
            Path::new("/proj/src/__pycache__/x.py"),
            root
        ));
        assert!(!is_python_source(Path::new("/proj/src/README.md"), root));
    }

    #[test]
    fn is_python_source_ignores_dirs_above_root() {
        // Repo rooted under a path containing "tests" must not exclude
        // everything — only components below root count.
        let root = Path::new("/home/ci/tests/myrepo");
        assert!(is_python_source(
            Path::new("/home/ci/tests/myrepo/src/calc.py"),
            root
        ));
        assert!(!is_python_source(
            Path::new("/home/ci/tests/myrepo/tests/test_x.py"),
            root
        ));
    }

    #[test]
    fn exclude_prunes_subdirs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("keep.py"), "x = 1 + 2\n").unwrap();
        fs::create_dir_all(root.join("alembic/versions")).unwrap();
        fs::write(root.join("alembic/env.py"), "x = 1 + 2\n").unwrap();
        fs::write(root.join("alembic/versions/001.py"), "x = 1 + 2\n").unwrap();

        let mutants = collect_from_tree(root, &["alembic/**".to_string()]).unwrap();
        for m in &mutants {
            let s = m.file.to_string_lossy();
            assert!(!s.contains("alembic"), "alembic leaked: {s}");
        }
        assert!(!mutants.is_empty(), "keep.py should still produce mutants");
    }

    #[test]
    fn exclude_matches_file_pattern() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("pkg/migrations")).unwrap();
        fs::write(root.join("pkg/core.py"), "x = 1 + 2\n").unwrap();
        fs::write(root.join("pkg/migrations/0001.py"), "x = 1 + 2\n").unwrap();

        let mutants = collect_from_tree(root, &["**/migrations/*.py".to_string()]).unwrap();
        for m in &mutants {
            let s = m.file.to_string_lossy();
            assert!(!s.contains("migrations"), "migrations leaked: {s}");
        }
    }

    #[test]
    fn empty_excludes_is_noop() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "x = 1 + 2\n").unwrap();
        let mutants = collect_from_tree(root, &[]).unwrap();
        assert!(!mutants.is_empty());
    }

    #[test]
    fn invalid_glob_errors() {
        let tmp = tempdir().unwrap();
        let err = collect_from_tree(tmp.path(), &["[unterminated".to_string()]).unwrap_err();
        assert!(err.to_string().contains("invalid exclude glob"));
    }
}
