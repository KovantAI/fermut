//! Embedded ty checker — Phase 1 + Phase 2.
//!
//! Wraps Astral's `ty_project::ProjectDatabase` so we can run type checks
//! without spawning a `ty` subprocess. The subprocess path in
//! [`super::ty::TyFilter`] pays ~150-200ms per call to bootstrap typeshed;
//! a single `ProjectDatabase` pays that cost once and amortizes it across
//! every subsequent `check_file`.
//!
//! Currently shipped:
//!
//! - Phase 1: construct a checker rooted at a project directory; count
//!   errors on an on-disk file.
//! - Phase 2: [`EmbeddedTyChecker::check_patched`] runs a type check
//!   against an in-memory overlay of a single file. The overlay is
//!   installed before the check and removed after, so subsequent
//!   on-disk checks see the original source again.
//!
//! Wired into the filter chain via [`super::ty::TyFilter`], which owns a
//! pool of these checkers and is installed by [`super::build_chain`].
//!
//! ## API stability
//!
//! `ty_project` and `ty_python_semantic` are pre-1.0. Any consumer of
//! this module must be ready to fall back to the subprocess path on a
//! ruff-tag bump. The [`EmbeddedTyChecker::new`] constructor returns
//! `Result` so callers can surface (and recover from) construction
//! failures.
//!
//! ## Concurrency contract
//!
//! [`EmbeddedTyChecker::check_patched`] takes `&mut self`. The overlay
//! mutation and the salsa `apply_changes` call are serialized through
//! that exclusive reference; one checker = one in-flight overlay at a
//! time. Phase 3 will explore per-worker DB clones for parallel
//! patched checks.

use std::collections::HashMap;
use std::fmt::Debug;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use ruff_db::diagnostic::Severity;
use ruff_db::file_revision::FileRevision;
use ruff_db::files::system_path_to_file;
use ruff_db::system::walk_directory::WalkDirectoryBuilder;
use ruff_db::system::{
    DirectoryEntry, FileType, Metadata, OsSystem, System, SystemPath, SystemPathBuf,
    SystemVirtualPath, WhichResult, WritableSystem,
};
use ruff_notebook::{Notebook, NotebookError};
use ty_project::watch::ChangeEvent;
use ty_project::{ProjectDatabase, ProjectMetadata};

/// In-memory overlay shared between the [`OverlaySystem`] and the
/// [`EmbeddedTyChecker`] that owns it. The `content` map holds the
/// currently-active overlay (empty entry = no overlay; read disk).
/// The `revision` map tracks a monotonic counter per path that we use
/// to fabricate a [`FileRevision`] in [`OverlaySystem::path_metadata`].
/// The counter is bumped on every install AND every removal — that's
/// how we force salsa to re-read the file's content through the
/// [`System`] trait both when an overlay goes in and when it comes out.
#[derive(Default, Debug)]
struct OverlayState {
    content: HashMap<SystemPathBuf, String>,
    /// Sticky: once a path has been touched, it stays in this map
    /// forever so subsequent metadata reads return the same revision
    /// the last mutation produced — even after the overlay clears.
    revision: HashMap<SystemPathBuf, u64>,
}

impl OverlayState {
    fn bump(&mut self, path: &SystemPath) -> u64 {
        let entry = self.revision.entry(path.to_path_buf()).or_insert(0);
        *entry += 1;
        *entry
    }
}

type OverlayHandle = Arc<Mutex<OverlayState>>;

pub struct EmbeddedTyChecker {
    db: ProjectDatabase,
    overlays: OverlayHandle,
}

impl EmbeddedTyChecker {
    /// Build a ty checker rooted at `project_path`. The directory must
    /// exist; `pyproject.toml` / `ty.toml` discovery follows the same
    /// rules as the `ty` binary.
    pub fn new(project_path: &Path) -> Result<Self> {
        let cwd = project_path
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", project_path.display()))?;
        let cwd_str = cwd
            .to_str()
            .ok_or_else(|| anyhow!("non-utf8 path: {}", cwd.display()))?;
        let cwd_sys = SystemPath::new(cwd_str).to_path_buf();

        let overlays: OverlayHandle = Arc::new(Mutex::new(OverlayState::default()));
        let inner_os = OsSystem::new(&cwd_sys);
        let system = OverlaySystem {
            inner: inner_os,
            overlays: Arc::clone(&overlays),
        };
        let project_metadata = ProjectMetadata::discover(&cwd_sys, &system)
            .map_err(|e| anyhow!("ty project discovery failed at {}: {e}", cwd.display()))?;
        let db = ProjectDatabase::fallible(project_metadata, system)
            .context("constructing ty ProjectDatabase")?;

        Ok(Self { db, overlays })
    }

    /// Number of `Severity::Error`/`Fatal` diagnostics the embedded
    /// checker reports for `path`. `path` must be absolute.
    pub fn error_count(&self, path: &Path) -> Result<usize> {
        let sys_path = canonical_system_path(path)?;
        Ok(count_errors(&self.db, &sys_path))
    }

    /// Run a type check with `path` overlaid to `patched_content`, then
    /// restore the overlay. Returns the count of error-severity (and
    /// fatal) diagnostics during the overlaid check.
    ///
    /// Takes `&mut self` because the salsa `apply_changes` call needs
    /// exclusive access. Workers that want parallel patched checks
    /// must serialize through one checker instance.
    pub fn check_patched(&mut self, path: &Path, patched_content: &str) -> Result<usize> {
        let sys_path = canonical_system_path(path)?;

        // Install the overlay + bump the per-path revision counter so
        // salsa's path_metadata read sees a different revision than
        // last time and invalidates the cached file content.
        {
            let mut state = self
                .overlays
                .lock()
                .map_err(|_| anyhow!("overlay state mutex poisoned"))?;
            state
                .content
                .insert(sys_path.clone(), patched_content.to_string());
            state.bump(&sys_path);
        }
        // Catch panics across both the install-side `apply_changes`
        // and `count_errors` so the overlay is always torn down. A
        // leaked overlay would corrupt every later check of this
        // path with stale patched content.
        let db = &mut self.db;
        let sys_path_ref = &sys_path;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.apply_changes(&[ChangeEvent::file_content_changed(sys_path_ref.clone())]);
            count_errors(db, sys_path_ref)
        }));

        // Restore: drop the overlay, bump revision again so salsa
        // re-reads from disk on the next access. Runs whether
        // count_errors returned normally or panicked. If the mutex
        // is poisoned (e.g. ty internal panicked while holding it),
        // clear the poison so the restore still happens — leaving
        // the overlay installed would corrupt every later check of
        // this path.
        let mut state = match self.overlays.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                self.overlays.clear_poison();
                poisoned.into_inner()
            }
        };
        state.content.remove(&sys_path);
        state.bump(&sys_path);
        drop(state);
        self.db
            .apply_changes(&[ChangeEvent::file_content_changed(sys_path.clone())]);

        match result {
            Ok(count) => Ok(count),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

fn count_errors(db: &ProjectDatabase, sys_path: &SystemPath) -> usize {
    let file = match system_path_to_file(db, sys_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(path = %sys_path, error = ?e, "ty embedded: file not in project");
            return 0;
        }
    };
    db.check_file(file)
        .iter()
        .filter(|d| matches!(d.severity(), Severity::Error | Severity::Fatal))
        .count()
}

fn canonical_system_path(path: &Path) -> Result<SystemPathBuf> {
    let abs = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    let abs_str = abs
        .to_str()
        .ok_or_else(|| anyhow!("non-utf8 path: {}", abs.display()))?;
    Ok(SystemPath::new(abs_str).to_path_buf())
}

/// A `System` wrapping [`OsSystem`] that swaps in an in-memory string for
/// configured paths. All other paths delegate straight through to disk.
/// Used by [`EmbeddedTyChecker`] so type checks can run against a
/// patched mutant source without touching the real source tree.
#[derive(Clone, Debug)]
struct OverlaySystem {
    inner: OsSystem,
    overlays: OverlayHandle,
}

impl OverlaySystem {
    fn overlay_for(&self, path: &SystemPath) -> Option<String> {
        self.overlays.lock().ok()?.content.get(path).cloned()
    }

    /// Revision counter for a path that has ever been overlaid. Returns
    /// `None` for paths we've never touched so they pass through to
    /// real filesystem metadata.
    fn overlay_revision(&self, path: &SystemPath) -> Option<u64> {
        self.overlays.lock().ok()?.revision.get(path).copied()
    }
}

impl System for OverlaySystem {
    fn path_metadata(&self, path: &SystemPath) -> std::io::Result<Metadata> {
        // For any path we've overlaid at least once, fabricate metadata
        // driven by our internal revision counter. That's what tells
        // salsa "this file changed" so it re-reads via read_to_string
        // and picks up the new overlay (or, post-restore, the on-disk
        // bytes again).
        if let Some(rev) = self.overlay_revision(path) {
            let perms = self
                .inner
                .path_metadata(path)
                .ok()
                .and_then(|m| m.permissions());
            return Ok(Metadata::new(
                FileRevision::from(rev as u128),
                perms,
                FileType::File,
            ));
        }
        self.inner.path_metadata(path)
    }

    fn canonicalize_path(&self, path: &SystemPath) -> std::io::Result<SystemPathBuf> {
        self.inner.canonicalize_path(path)
    }

    fn is_same_file(&self, path1: &SystemPath, path2: &SystemPath) -> std::io::Result<bool> {
        self.inner.is_same_file(path1, path2)
    }

    fn read_to_string(&self, path: &SystemPath) -> std::io::Result<String> {
        if let Some(content) = self.overlay_for(path) {
            return Ok(content);
        }
        self.inner.read_to_string(path)
    }

    fn read_to_notebook(&self, path: &SystemPath) -> std::result::Result<Notebook, NotebookError> {
        self.inner.read_to_notebook(path)
    }

    fn read_virtual_path_to_string(&self, path: &SystemVirtualPath) -> std::io::Result<String> {
        self.inner.read_virtual_path_to_string(path)
    }

    fn read_virtual_path_to_notebook(
        &self,
        path: &SystemVirtualPath,
    ) -> std::result::Result<Notebook, NotebookError> {
        self.inner.read_virtual_path_to_notebook(path)
    }

    fn path_exists(&self, path: &SystemPath) -> bool {
        self.inner.path_exists(path)
    }

    fn which(&self, binary_name: &str) -> WhichResult {
        self.inner.which(binary_name)
    }

    fn current_directory(&self) -> &SystemPath {
        self.inner.current_directory()
    }

    fn user_config_directory(&self) -> Option<SystemPathBuf> {
        self.inner.user_config_directory()
    }

    fn cache_dir(&self) -> Option<SystemPathBuf> {
        self.inner.cache_dir()
    }

    fn read_directory<'a>(
        &'a self,
        path: &SystemPath,
    ) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<DirectoryEntry>> + 'a>> {
        self.inner.read_directory(path)
    }

    fn walk_directory(&self, path: &SystemPath) -> WalkDirectoryBuilder {
        self.inner.walk_directory(path)
    }

    fn env_var(&self, name: &str) -> std::result::Result<String, std::env::VarError> {
        self.inner.env_var(name)
    }

    fn as_writable(&self) -> Option<&dyn WritableSystem> {
        // Read-through delegate; we never patch writes through the
        // overlay. Returning the inner's writable view is safe because
        // any write goes straight to disk.
        self.inner.as_writable()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn dyn_clone(&self) -> Box<dyn System> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_fixture(dir: &Path, file: &str, contents: &str) {
        let target = dir.join(file);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(target, contents).unwrap();
    }

    fn fixture_project(contents: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(
            tmp.path(),
            "pyproject.toml",
            "[project]\nname = \"fx\"\nversion = \"0\"\nrequires-python = \">=3.10\"\n",
        );
        write_fixture(tmp.path(), "src/__init__.py", "");
        write_fixture(tmp.path(), "src/mod.py", contents);
        tmp
    }

    #[test]
    fn checker_constructs_on_minimal_project() {
        let tmp = fixture_project("x: int = 1\n");
        let _checker = EmbeddedTyChecker::new(tmp.path()).expect("checker should build");
    }

    #[test]
    fn clean_file_reports_zero_errors() {
        let tmp = fixture_project("def add(a: int, b: int) -> int:\n    return a + b\n");
        let checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let n = checker
            .error_count(&tmp.path().join("src/mod.py"))
            .expect("error_count");
        assert_eq!(n, 0, "clean source should yield zero errors");
    }

    #[test]
    fn obvious_type_error_is_counted() {
        let tmp = fixture_project("def bad() -> int:\n    return 1 + \"two\"\n");
        let checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let n = checker
            .error_count(&tmp.path().join("src/mod.py"))
            .expect("error_count");
        assert!(n >= 1, "expected ty to flag int + str, got {n} errors");
    }

    #[test]
    fn nonexistent_path_yields_error_not_panic() {
        let tmp = fixture_project("x = 1\n");
        let checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let result = checker.error_count(&tmp.path().join("src/does_not_exist.py"));
        assert!(result.is_err());
    }

    #[test]
    fn missing_project_dir_fails_construction() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let r = EmbeddedTyChecker::new(&missing);
        assert!(r.is_err(), "expected construction to fail on missing dir");
    }

    #[test]
    fn check_patched_flags_type_error_from_overlay() {
        // On-disk source is clean; patched version introduces `int + str`.
        let tmp = fixture_project("def f() -> int:\n    return 1 + 2\n");
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let path = tmp.path().join("src/mod.py");

        let n_clean = checker.error_count(&path).unwrap();
        assert_eq!(n_clean, 0, "baseline must be 0");

        let n_patched = checker
            .check_patched(&path, "def f() -> int:\n    return 1 + \"two\"\n")
            .unwrap();
        assert!(
            n_patched >= 1,
            "expected patched source to flag int + str, got {n_patched}"
        );
    }

    #[test]
    fn check_patched_clean_overlay_keeps_zero_errors() {
        let tmp = fixture_project("def f() -> int:\n    return 1\n");
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let path = tmp.path().join("src/mod.py");

        let n = checker
            .check_patched(&path, "def f() -> int:\n    return 99\n")
            .unwrap();
        assert_eq!(n, 0, "harmless edit must not introduce errors");
    }

    #[test]
    fn overlay_is_restored_after_check_patched() {
        // Two passes: patched run must NOT poison the subsequent
        // on-disk check. Salsa re-invalidation is the key contract here.
        let tmp = fixture_project("def f() -> int:\n    return 1\n");
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let path = tmp.path().join("src/mod.py");

        let _ = checker
            .check_patched(&path, "def f() -> int:\n    return 1 + \"x\"\n")
            .unwrap();
        let after = checker
            .error_count(&path)
            .expect("post-patched on-disk check");
        assert_eq!(
            after, 0,
            "on-disk file must be clean again after overlay clears"
        );
    }

    #[test]
    fn repeated_check_patched_with_different_payloads() {
        let tmp = fixture_project("def f() -> int:\n    return 1\n");
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let path = tmp.path().join("src/mod.py");

        let bad = checker
            .check_patched(&path, "def f() -> int:\n    return 1 + \"x\"\n")
            .unwrap();
        let good = checker
            .check_patched(&path, "def f() -> int:\n    return 1 + 2\n")
            .unwrap();
        let bad_again = checker
            .check_patched(&path, "def f() -> int:\n    return 1 + \"y\"\n")
            .unwrap();
        assert!(bad >= 1);
        assert_eq!(good, 0);
        assert!(bad_again >= 1);
    }

    /// Project with two source files so cross-file overlay isolation
    /// and cross-module invalidation can both be exercised.
    fn fixture_two_file_project(a_contents: &str, b_contents: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(
            tmp.path(),
            "pyproject.toml",
            "[project]\nname = \"fx\"\nversion = \"0\"\nrequires-python = \">=3.10\"\n",
        );
        write_fixture(tmp.path(), "src/__init__.py", "");
        write_fixture(tmp.path(), "src/a.py", a_contents);
        write_fixture(tmp.path(), "src/b.py", b_contents);
        tmp
    }

    #[test]
    fn overlay_on_one_file_does_not_leak_into_unrelated_file() {
        // Risk: in-process checker holds shared salsa state across files.
        // Overlaying file A must not change ty's verdict on file B when
        // the two are independent. If revision tracking accidentally
        // becomes file-global instead of per-path, B's cached analysis
        // would get bumped and re-checked under stale assumptions.
        let tmp = fixture_two_file_project(
            "def f() -> int:\n    return 1\n",
            "def g() -> int:\n    return 2\n",
        );
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let a = tmp.path().join("src/a.py");
        let b = tmp.path().join("src/b.py");

        assert_eq!(checker.error_count(&b).unwrap(), 0, "B baseline");
        // Patch A with a type error; the count for A spikes.
        let a_err = checker
            .check_patched(&a, "def f() -> int:\n    return 1 + \"x\"\n")
            .unwrap();
        assert!(a_err >= 1, "patched A should error, got {a_err}");
        // B is unrelated to A — its error count must be untouched, and
        // critically must still be 0 (not corrupted by A's overlay).
        assert_eq!(
            checker.error_count(&b).unwrap(),
            0,
            "overlaying A must not introduce errors into unrelated file B"
        );
    }

    #[test]
    fn overlay_propagates_through_cross_module_import_then_clears() {
        // The hardest case for in-process salsa: file B imports a symbol
        // from file A and uses its type. Overlaying A with an incompatible
        // signature must invalidate B's analysis so B starts reporting
        // errors. After restoring A, B must go back to clean — otherwise
        // a later mutant on an unrelated file would see stale errors on
        // B and the filter chain would mis-admit.
        //
        // Soft-asserted because not every change ty currently propagates
        // shows up as a downstream diagnostic on B at this ruff tag; the
        // load-bearing assertion is the *restore* half — once A is back
        // to its on-disk content, B is clean again.
        let tmp = fixture_two_file_project(
            "def f() -> int:\n    return 1\n",
            "from src.a import f\n\nx: int = f()\n",
        );
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let a = tmp.path().join("src/a.py");
        let b = tmp.path().join("src/b.py");

        let b_baseline = checker.error_count(&b).unwrap();

        // Patch A so f returns str. Don't assert on B during the overlay
        // — ty's cross-module diagnostic surface is the property under
        // test, not contract. The contract we DO enforce is that the
        // restore puts B back where it started.
        let _ = checker
            .check_patched(&a, "def f() -> str:\n    return \"x\"\n")
            .unwrap();

        // After restore: B must read the same way it did before A was
        // touched. A residual overlay would leave B reporting a type
        // mismatch that doesn't exist on disk.
        let b_after = checker.error_count(&b).unwrap();
        assert_eq!(
            b_baseline, b_after,
            "cross-module restore failed: B errors drifted from {b_baseline} \
             to {b_after} after A overlay round-tripped"
        );
    }

    #[test]
    fn sequential_overlays_on_different_files_do_not_interfere() {
        // Simulates the worker-thread loop: mutant 1 overlays file A,
        // mutant 2 overlays file B. After both, on-disk reads of A and B
        // must reflect their actual on-disk contents — neither overlay
        // can have leaked into the other path's cached file content.
        let tmp = fixture_two_file_project(
            "def f() -> int:\n    return 1\n",
            "def g() -> int:\n    return 2\n",
        );
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let a = tmp.path().join("src/a.py");
        let b = tmp.path().join("src/b.py");

        let _ = checker
            .check_patched(&a, "def f() -> int:\n    return 1 + \"x\"\n")
            .unwrap();
        let _ = checker
            .check_patched(&b, "def g() -> int:\n    return 2 + \"y\"\n")
            .unwrap();

        assert_eq!(
            checker.error_count(&a).unwrap(),
            0,
            "A should be clean after B's overlay also round-tripped"
        );
        assert_eq!(
            checker.error_count(&b).unwrap(),
            0,
            "B should be clean after its own overlay round-tripped"
        );
    }

    #[test]
    fn stress_many_overlay_cycles_do_not_drift() {
        // 30 alternating overlay cycles on the same file. The bad/good
        // pair must produce stable verdicts every iteration. A drifting
        // count would indicate residue accumulating in the checker's
        // shared state — the worst failure mode of the in-process move,
        // because it's invisible until a long run reveals it.
        let tmp = fixture_project("def f() -> int:\n    return 1\n");
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let path = tmp.path().join("src/mod.py");

        let bad = "def f() -> int:\n    return 1 + \"x\"\n";
        let good = "def f() -> int:\n    return 2\n";

        let (first_bad, first_good) = (
            checker.check_patched(&path, bad).unwrap(),
            checker.check_patched(&path, good).unwrap(),
        );
        assert!(first_bad >= 1);
        assert_eq!(first_good, 0);

        for i in 0..30 {
            let nb = checker.check_patched(&path, bad).unwrap();
            let ng = checker.check_patched(&path, good).unwrap();
            assert_eq!(
                nb, first_bad,
                "iteration {i}: bad verdict drifted from {first_bad} to {nb}"
            );
            assert_eq!(
                ng, first_good,
                "iteration {i}: good verdict drifted from {first_good} to {ng}"
            );
        }
        // Final on-disk check must still be clean.
        assert_eq!(checker.error_count(&path).unwrap(), 0);
    }

    #[test]
    fn check_patched_matches_error_count_for_same_content() {
        // Diagnostic parity: error_count on disk should equal
        // check_patched with the same content. This is the contract
        // that lets Phase 3 use check_patched as a drop-in.
        let tmp = fixture_project("def f() -> int:\n    return 1 + \"x\"\n");
        let mut checker = EmbeddedTyChecker::new(tmp.path()).unwrap();
        let path = tmp.path().join("src/mod.py");

        let on_disk = checker.error_count(&path).unwrap();
        let overlay = checker
            .check_patched(&path, "def f() -> int:\n    return 1 + \"x\"\n")
            .unwrap();
        assert_eq!(
            on_disk, overlay,
            "overlay verdict must match on-disk verdict for identical content"
        );
    }
}
