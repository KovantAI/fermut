//! What a mirror held when it was built, to catch tests that leave it changed.
//!
//! A worker reuses one mirror for every mutant, and a mutant makes tests fail
//! midway: a test that moves a fixture away and back, or creates a file and
//! removes it after an assertion, skips its cleanup, and the file stays moved
//! for every later mutant on that worker (aiofiles' `test_rename`: later
//! mutants "killed" by `FileNotFoundError` in `test_stat`, ~130 verdicts
//! flipping run to run). [`Pristine::restore`] puts such files back after each
//! mutant; the schemata runner, whose one mirror every fork shares, uses
//! [`Pristine::drift`] to stop serving instead.
//!
//! "Pristine" means what a passing run leaves, not what the build copied: an
//! app that creates its instance or upload dirs on every start (flaskbb's
//! `os.makedirs(app.instance_path, exist_ok=True)`), gitignored and so left
//! out of the mirror, isn't a test skipping its cleanup. The baseline records
//! the paths it added ([`record_clean_run`]); every later snapshot of that
//! project lets them be.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::config::IsolationMode;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stamp {
    Dir,
    File(u64, Option<SystemTime>),
    /// A symlink or anything else; a built mirror holds none.
    Other,
}

/// Per project root (`Mirror::project_root`): the paths a passing baseline
/// added to its mirror. Keyed so one process running several projects
/// (`fermut mcp`) keeps them apart.
static CLEAN_RUN: LazyLock<Mutex<HashMap<PathBuf, Arc<HashSet<PathBuf>>>>> =
    LazyLock::new(Mutex::default);

/// Remember what a passing baseline added to its mirror since `before` was
/// taken, for every later snapshot of `project_root`.
pub(crate) fn record_clean_run(project_root: &Path, before: &Pristine) {
    let added = scan(&before.root)
        .into_keys()
        .filter(|p| !before.entries.contains_key(p))
        .collect();
    crate::sync::lock_recover(&CLEAN_RUN).insert(project_root.to_path_buf(), Arc::new(added));
}

fn clean_run(project_root: &Path) -> Arc<HashSet<PathBuf>> {
    crate::sync::lock_recover(&CLEAN_RUN)
        .get(project_root)
        .cloned()
        .unwrap_or_default()
}

pub(crate) struct Pristine {
    root: PathBuf,
    entries: HashMap<PathBuf, Stamp>,
    /// Paths a passing run of this project adds (see [`record_clean_run`]):
    /// not drift while the build didn't have them.
    clean: Arc<HashSet<PathBuf>>,
}

impl Pristine {
    /// Snapshot the tree under `root`, a mirror of `project_root`. Taken once
    /// the mirror is complete (precompiled, schematized).
    pub(crate) fn take(root: &Path, project_root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            entries: scan(root),
            clean: clean_run(project_root),
        }
    }

    /// Paths (relative to the root) that were added, removed, or changed size
    /// or mtime since the snapshot, sorted (a parent before its children).
    /// Paths a passing run adds don't count.
    pub(crate) fn drift(&self) -> Vec<PathBuf> {
        let now = scan(&self.root);
        let mut out: Vec<PathBuf> = now
            .iter()
            .filter(|(p, s)| self.entries.get(*p) != Some(s))
            .filter(|(p, _)| self.entries.contains_key(*p) || !self.clean.contains(*p))
            .map(|(p, _)| p.clone())
            .chain(
                self.entries
                    .keys()
                    .filter(|p| !now.contains_key(*p))
                    .cloned(),
            )
            .collect();
        out.sort();
        out
    }

    /// Put every drifted path back the way the build left it, copying files
    /// from `source` (the project the mirror was copied from) and deleting
    /// what wasn't there. Returns the paths whose content had changed. A file
    /// only re-written with the same bytes (the mutated file, reverted by its
    /// patch guard) isn't one: its stamp is just refreshed.
    pub(crate) fn restore(&mut self, source: &Path, mode: IsolationMode) -> Result<Vec<PathBuf>> {
        let mut dirty = Vec::new();
        for rel in self.drift() {
            let at = self.root.join(&rel);
            let was = self.entries.get(&rel).copied();
            if was.is_some_and(|s| matches!(s, Stamp::File(..)))
                && stamp(&at) != Stamp::Dir
                && same_bytes(&at, &source.join(&rel))
            {
                self.entries.insert(rel, stamp(&at));
                continue;
            }
            // A child of a path already put back is gone or restored by now.
            if !dirty.iter().any(|d| rel.starts_with(d)) {
                dirty.push(rel.clone());
            }
            match std::fs::symlink_metadata(&at) {
                Ok(m) if m.is_dir() && was != Some(Stamp::Dir) => std::fs::remove_dir_all(&at),
                Ok(m) if !m.is_dir() => std::fs::remove_file(&at),
                _ => Ok(()),
            }
            .with_context(|| format!("removing {}", at.display()))?;
            match was {
                Some(Stamp::Dir) => std::fs::create_dir_all(&at)
                    .with_context(|| format!("mkdir -p {}", at.display()))?,
                Some(Stamp::File(..)) => {
                    if let Some(parent) = at.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("mkdir -p {}", parent.display()))?;
                    }
                    super::clone_file(&source.join(&rel), &at, mode)?;
                }
                _ => {}
            }
            if was.is_some() {
                self.entries.insert(rel, stamp(&at));
            }
        }
        Ok(dirty)
    }
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

fn stamp(p: &Path) -> Stamp {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => Stamp::Dir,
        Ok(m) if m.is_file() => Stamp::File(m.len(), m.modified().ok()),
        _ => Stamp::Other,
    }
}

/// Every path under `root` but bytecode caches and the tool dirs runs create
/// (`.pytest_cache`, `.hypothesis`; see `is_pruned_dir_name`), which the build
/// leaves out and no test reads back.
fn scan(root: &Path) -> HashMap<PathBuf, Stamp> {
    let mut out = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let s = stamp(&path);
            if s == Stamp::Dir {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(crate::util::is_pruned_dir_name)
                {
                    continue;
                }
                stack.push(path.clone());
            }
            out.insert(path.strip_prefix(root).unwrap().to_path_buf(), s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_undoes_moves_leftovers_and_rewrites() {
        let src = tempfile::tempdir().unwrap();
        let res = src.path().join("tests/resources");
        std::fs::create_dir_all(&res).unwrap();
        std::fs::write(res.join("f1.txt"), "one").unwrap();
        std::fs::write(src.path().join("mod.py"), "x = 1").unwrap();
        let mirror = tempfile::tempdir().unwrap();
        let root = mirror.path();
        super::super::copy_dir_all(src.path(), root, IsolationMode::Copy).unwrap();
        let mut p = Pristine::take(root, src.path());
        assert!(p.drift().is_empty());

        // A test failed between its moves, another left a dir and a cache.
        std::fs::rename(
            root.join("tests/resources/f1.txt"),
            root.join("tests/resources/f2.txt"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("tests/resources/test_dir/sub")).unwrap();
        std::fs::create_dir_all(root.join(".pytest_cache")).unwrap();
        // The patch guard reverted the mutated file: same bytes, new inode.
        std::fs::remove_file(root.join("mod.py")).unwrap();
        std::fs::write(root.join("mod.py"), "x = 1").unwrap();

        let dirty = p.restore(src.path(), IsolationMode::Copy).unwrap();
        let rel = |s: &str| PathBuf::from(s);
        assert_eq!(
            dirty,
            [
                rel("tests/resources/f1.txt"),
                rel("tests/resources/f2.txt"),
                rel("tests/resources/test_dir")
            ]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("tests/resources/f1.txt")).unwrap(),
            "one"
        );
        assert!(!root.join("tests/resources/f2.txt").exists());
        assert!(!root.join("tests/resources/test_dir").exists());
        assert!(p.drift().is_empty(), "{:?}", p.drift());

        // A file a test rewrote in place gets its content back.
        std::fs::write(root.join("mod.py"), "x = 2").unwrap();
        assert_eq!(
            p.restore(src.path(), IsolationMode::Copy).unwrap(),
            [rel("mod.py")]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("mod.py")).unwrap(),
            "x = 1"
        );
    }

    #[test]
    fn restore_recreates_a_removed_dir_and_its_files() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("data/empty")).unwrap();
        std::fs::write(src.path().join("data/f.txt"), "one").unwrap();
        let mirror = tempfile::tempdir().unwrap();
        let root = mirror.path();
        super::super::copy_dir_all(src.path(), root, IsolationMode::Copy).unwrap();
        let mut p = Pristine::take(root, src.path());

        std::fs::remove_dir_all(root.join("data")).unwrap();
        let dirty = p.restore(src.path(), IsolationMode::Copy).unwrap();
        assert_eq!(dirty, [PathBuf::from("data")]);
        assert!(root.join("data/empty").is_dir());
        assert_eq!(
            std::fs::read_to_string(root.join("data/f.txt")).unwrap(),
            "one"
        );
        assert!(p.drift().is_empty(), "{:?}", p.drift());
    }

    /// #84: an app that creates its instance and upload dirs on every start
    /// (gitignored, so left out of the mirror) leaves them after a passing run
    /// too. Once the baseline recorded them they aren't drift, and restore
    /// leaves them be; a leftover the passing run doesn't make still is.
    #[test]
    fn paths_a_passing_run_adds_are_not_drift() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("tests/resources")).unwrap();
        std::fs::write(src.path().join("tests/resources/f1.txt"), "one").unwrap();
        let mirror = || {
            let dir = tempfile::tempdir().unwrap();
            super::super::copy_dir_all(src.path(), dir.path(), IsolationMode::Copy).unwrap();
            dir
        };
        let app_start = |root: &Path| {
            std::fs::create_dir_all(root.join("instance")).unwrap();
            std::fs::create_dir_all(root.join("static/uploads/avatar")).unwrap();
            std::fs::write(root.join("instance/app.db"), "db").unwrap();
        };

        // The baseline: snapshot its mirror, a passing run, record.
        let base = mirror();
        let before = Pristine::take(base.path(), src.path());
        app_start(base.path());
        record_clean_run(src.path(), &before);

        let work = mirror();
        let root = work.path();
        let mut p = Pristine::take(root, src.path());
        app_start(root);
        assert!(p.drift().is_empty(), "{:?}", p.drift());
        std::fs::write(root.join("instance/app.db"), "db, written again").unwrap();
        assert!(p.drift().is_empty(), "{:?}", p.drift());

        std::fs::remove_file(root.join("tests/resources/f1.txt")).unwrap();
        std::fs::create_dir_all(root.join("tests/resources/test_dir")).unwrap();
        let rel = |s: &str| PathBuf::from(s);
        assert_eq!(
            p.restore(src.path(), IsolationMode::Copy).unwrap(),
            [
                rel("tests/resources/f1.txt"),
                rel("tests/resources/test_dir")
            ]
        );
        assert!(root.join("tests/resources/f1.txt").is_file());
        assert!(root.join("instance/app.db").is_file());
        assert!(root.join("static/uploads/avatar").is_dir());

        // A project with no passing baseline recorded: the same dirs drift.
        let other = mirror();
        let q = Pristine::take(other.path(), &other.path().join("unrecorded"));
        app_start(other.path());
        assert!(q.drift().contains(&rel("instance")), "{:?}", q.drift());
    }

    #[test]
    fn an_unreadable_root_scans_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Pristine::take(&tmp.path().join("missing"), tmp.path());
        assert!(p.entries.is_empty());
        assert!(p.drift().is_empty());
    }
}
