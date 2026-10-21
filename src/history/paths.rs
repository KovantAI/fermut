//! Locating the project's `.fermut/history.jsonl` from a target path.

use std::path::{Path, PathBuf};

/// Default history location: sibling of the cache file, anchored to the
/// project's `.fermut/` directory.
pub fn default_history_path(project_root: &Path) -> PathBuf {
    project_root.join(".fermut").join("history.jsonl")
}

/// Resolve the directory whose `.fermut/` holds this project's history log.
///
/// `fermut run <path>` and `fermut trend` are usually handed different
/// `<path>` args (or none at all), yet must agree on one project-level
/// timeline. Anchoring on the raw path arg broke that: `fermut run src/`
/// wrote `src/.fermut/history.jsonl` while a bare `fermut trend` read
/// `./.fermut/history.jsonl` and saw an empty history. Both now walk up to a
/// shared anchor:
///   1. the nearest ancestor (including the target) holding a project marker
///      (`pyproject.toml` / `setup.cfg`);
///   2. failing that, the nearest ancestor that already has a `.fermut/`
///      directory — so `trend` finds wherever a prior `run` wrote;
///   3. failing both, the target directory itself.
///
/// A file target resolves against its parent directory (you can't put a
/// `.fermut/` under a `.py` file).
pub fn resolve_root(path: &Path) -> PathBuf {
    let abs = absolutize(path);
    let base = if abs.is_file() {
        abs.parent().map(Path::to_path_buf).unwrap_or(abs)
    } else {
        abs
    };
    if let Some(root) = find_ancestor(&base, |p| {
        p.join("pyproject.toml").exists() || p.join("setup.cfg").exists()
    }) {
        return root;
    }
    if let Some(root) = find_ancestor(&base, |p| p.join(".fermut").is_dir()) {
        return root;
    }
    base
}

/// Nearest ancestor of `start` (inclusive) satisfying `pred`, or `None`.
fn find_ancestor(start: &Path, pred: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut p = start;
    loop {
        if pred(p) {
            return Some(p.to_path_buf());
        }
        p = p.parent()?;
    }
}

/// Best-effort absolute path: canonicalize when it exists, else join cwd.
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    std::env::current_dir()
        .map(|d| d.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_history_path_uses_dot_fermut() {
        let p = default_history_path(Path::new("/proj"));
        assert!(p.ends_with(".fermut/history.jsonl"));
    }

    #[test]
    fn resolve_root_agrees_for_run_subpath_and_bare_trend() {
        // The regression: `fermut run src/` and a bare `fermut trend` (path
        // `.`) must resolve to the same project root so trend reads what run
        // wrote. Anchor is the pyproject.toml marker at the project root.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();

        let from_run = resolve_root(&src);
        let from_trend = resolve_root(&root);
        assert_eq!(
            from_run, from_trend,
            "run-on-src and trend-on-root must land on the same project root"
        );
        assert_eq!(from_run, root);
    }

    #[test]
    fn resolve_root_falls_back_to_existing_dot_fermut() {
        // No project marker, but a prior run left a `.fermut/` at the root.
        // A subpath lookup must climb to it instead of anchoring on the
        // subdir, so trend finds the existing history.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".fermut")).unwrap();
        let src = root.join("pkg");
        std::fs::create_dir_all(&src).unwrap();

        assert_eq!(resolve_root(&src), root);
    }

    #[test]
    fn resolve_root_file_target_uses_parent() {
        // No marker, no prior `.fermut/`: a single-file target resolves to its
        // parent directory, never to `file.py/.fermut`.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let file = root.join("mod.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        assert_eq!(resolve_root(&file), root);
    }
}
