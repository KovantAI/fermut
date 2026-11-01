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
