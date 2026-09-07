//! Python interpreter discovery for the pytest runner.
//!
//! fermut historically spawned a bare `pytest` resolved from `PATH`. That
//! breaks in environments that won't let you put the venv's `bin/` on PATH
//! (locked-down agent sandboxes, some CI), and is ambiguous when several
//! interpreters are around. `--python` (or the `python` config key) lets the
//! caller name an interpreter or a virtualenv; fermut then invokes
//! `<python> -m pytest` with an absolute interpreter, no PATH dependency.
//!
//! Discovery is deliberately lightweight — venvs only, no version grammar or
//! managed-toolchain scan (cf. rstest's richer resolver). The rule: only
//! surface an interpreter that almost certainly has `pytest` installed, so an
//! auto-picked one won't fail under `-m pytest`. A bare system `python3` may
//! lack pytest, so it's left to the existing `pytest`-on-PATH fallback
//! (`resolve_python` returns `None`).

use std::path::{Path, PathBuf};

/// Resolve the interpreter to run pytest with, or `None` to fall back to a
/// bare `pytest` on `PATH` (the historical behavior).
///
/// `explicit` is the `--python` value / `python` config key: either an
/// interpreter path or a virtualenv directory. It is **authoritative** — once
/// given, we resolve it (a venv dir maps to its interpreter) and return it
/// even if it looks wrong, so a typo surfaces as a clear spawn error instead
/// of a silent fallback to a different interpreter.
///
/// Without `explicit`, discover a virtualenv whose interpreter is very likely
/// to have pytest: the active `VIRTUAL_ENV`, then a `.venv` found walking up
/// from `scope` (stopping at a repo root). Returns `None` when none is found.
pub fn resolve_python(scope: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(resolve_explicit(p));
    }
    if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
        if let Some(py) = venv_python(Path::new(&venv)) {
            return Some(py);
        }
    }
    discover_dot_venv(scope)
}

/// Resolve an executable `name` (e.g. `ty`, `ruff`) to an absolute path,
/// preferring the project's virtualenv `bin/` — derived from `scope` via
/// [`resolve_python`] — and falling back to the ambient `PATH`.
///
/// This is the single tool-resolution used by both the filter runners
/// (`filter::ty`, `filter::ruff`) and `fermut doctor`, so doctor's verdict
/// ("ty ok") matches what a real run resolves. Previously the filters shelled
/// out to `which <name>` (searching ambient PATH only) while doctor prepended
/// the venv `bin/`, so doctor could report a tool present that the run then
/// failed to find. It also fixes Windows, where there is no `which` binary —
/// the `which` crate handles `.exe`/`PATHEXT` and the platform PATH split.
pub fn resolve_tool(name: &str, scope: &Path) -> Option<PathBuf> {
    let venv_bin = resolve_python(scope, None)
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    resolve_tool_in(name, venv_bin.as_deref())
}

/// Env-free core of [`resolve_tool`]: search `venv_bin` (when given) ahead of
/// the ambient `PATH`. Split out so the precedence is unit-testable without
/// mutating process-global `VIRTUAL_ENV`/`PATH`.
fn resolve_tool_in(name: &str, venv_bin: Option<&Path>) -> Option<PathBuf> {
    let ambient = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(bin) = venv_bin {
        dirs.push(bin.to_path_buf());
    }
    dirs.extend(std::env::split_paths(&ambient));
    let search = std::env::join_paths(dirs).ok()?;
    let cwd = std::env::current_dir().ok()?;
    which::which_in(name, Some(search), cwd).ok()
}

/// Walk up from `scope` looking for a `.venv` interpreter, stopping at the
/// repo root so we never reach a `.venv` outside the project. Env-free, so
/// it's unit-testable without touching `VIRTUAL_ENV`.
fn discover_dot_venv(scope: &Path) -> Option<PathBuf> {
    for dir in scope.ancestors() {
        if let Some(py) = venv_python(&dir.join(".venv")) {
            return Some(py);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// Map an explicit `--python` value to an interpreter path. A virtualenv
/// directory resolves to its interpreter; anything else is taken verbatim.
fn resolve_explicit(p: &Path) -> PathBuf {
    venv_python(p).unwrap_or_else(|| p.to_path_buf())
}

/// `<venv>/bin/python[3]` (unix) or `<venv>/Scripts/python.exe` (Windows), if
/// it exists. `None` when `venv` isn't a virtualenv (or doesn't exist).
fn venv_python(venv: &Path) -> Option<PathBuf> {
    for rel in ["bin/python", "bin/python3", "Scripts/python.exe"] {
        let p = venv.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fermut-py-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn make_venv(dir: &Path) -> PathBuf {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let py = bin.join("python");
        fs::write(&py, "").unwrap();
        py
    }

    #[test]
    fn explicit_interpreter_path_passthrough() {
        // A path that isn't a venv dir is taken verbatim — even if absent, so
        // the spawn error names it.
        let p = Path::new("/opt/py/bin/python3.12");
        assert_eq!(
            resolve_python(Path::new("/scope"), Some(p)),
            Some(p.to_path_buf())
        );
    }

    #[test]
    fn explicit_venv_dir_resolves_to_interpreter() {
        let root = tmp("explicit-venv");
        let venv = root.join("myenv");
        let py = make_venv(&venv);
        assert_eq!(resolve_python(&root, Some(&venv)), Some(py));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discovers_dot_venv_walking_up_and_stops_at_repo_root() {
        let root = tmp("walk");
        let repo = root.join("repo");
        let nested = repo.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        let py = make_venv(&repo.join(".venv"));
        // A .venv above the repo root must NOT be reached.
        make_venv(&root.join(".venv"));

        assert_eq!(discover_dot_venv(&nested), Some(py));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dot_venv_walk_stops_at_repo_root_returns_none() {
        let root = tmp("walk-none");
        let repo = root.join("repo");
        let nested = repo.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        // Only a .venv ABOVE the repo root exists — the walk must stop at .git
        // and never reach it.
        make_venv(&root.join(".venv"));

        assert_eq!(discover_dot_venv(&nested), None);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Write an executable file at `path` (0o755 on Unix so `which` accepts it).
    fn make_exe(path: &Path) {
        fs::write(path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn resolve_tool_prefers_venv_bin_over_path() {
        let root = tmp("resolve-venv-first");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        // On Windows `which` needs the executable extension.
        let exe = if cfg!(windows) {
            "faketool.exe"
        } else {
            "faketool"
        };
        let tool = bin.join(exe);
        make_exe(&tool);

        assert_eq!(resolve_tool_in("faketool", Some(&bin)), Some(tool));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_tool_missing_returns_none() {
        // A name that exists in neither the (empty) venv bin nor PATH.
        let root = tmp("resolve-missing");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();

        assert_eq!(
            resolve_tool_in("fermut-nonexistent-tool-xyz", Some(&bin)),
            None
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
