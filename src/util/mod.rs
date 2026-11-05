//! Small, dependency-free utilities shared across subcommands.

pub(crate) mod search;

/// Directories never copied into the worker mirror or hashed for its
/// fingerprints: VCS metadata, virtualenvs, tool caches, agent state, and
/// `__pycache__` (stale bytecode masks mutations). `.gitignore` drops the rest
/// of the heavy stuff, so other dot-dirs (`.github`, `.config`) are kept —
/// tests may read files from them.
pub(crate) fn is_pruned_dir_name(name: &str) -> bool {
    matches!(
        name,
        "__pycache__"
            | ".git"
            | ".hg"
            | ".svn"
            | ".venv"
            | ".direnv"
            | ".tox"
            | ".nox"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".ruff_cache"
            | ".hypothesis"
            | ".fermut"
            | ".claude"
    )
}

/// `p` for human output: relative to the current directory when it sits
/// under it (`src/pkg/mod.py`), else unchanged. Mutant paths are absolute and
/// canonical (mutant ids depend on that), which makes every report line carry
/// the full checkout path.
pub(crate) fn display_path(p: &std::path::Path) -> std::path::Display<'_> {
    static CWD: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let cwd = CWD.get_or_init(|| std::env::current_dir().ok()?.canonicalize().ok());
    relative_under(p, cwd.as_deref()).display()
}

fn relative_under<'a>(
    p: &'a std::path::Path,
    base: Option<&std::path::Path>,
) -> &'a std::path::Path {
    // Under the filesystem root everything is "inside"; `private/tmp/..`
    // would only read as a broken relative path.
    let base = base.filter(|b| b.parent().is_some());
    match base.and_then(|b| p.strip_prefix(b).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel,
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::relative_under;
    use std::path::Path;

    #[test]
    fn relative_under_strips_base_only_when_inside_it() {
        let base = Some(Path::new("/repo"));
        assert_eq!(
            relative_under(Path::new("/repo/src/a.py"), base),
            Path::new("src/a.py")
        );
        assert_eq!(
            relative_under(Path::new("/other/a.py"), base),
            Path::new("/other/a.py")
        );
        // Sibling dir sharing a name prefix is not "inside".
        assert_eq!(
            relative_under(Path::new("/repo2/a.py"), base),
            Path::new("/repo2/a.py")
        );
        assert_eq!(relative_under(Path::new("/repo"), base), Path::new("/repo"));
        assert_eq!(
            relative_under(Path::new("/repo/a.py"), None),
            Path::new("/repo/a.py")
        );
        assert_eq!(
            relative_under(Path::new("/repo/a.py"), Some(Path::new("/"))),
            Path::new("/repo/a.py")
        );
    }
}
