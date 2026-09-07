//! Git discovery for history entries: short sha + current branch. Failures
//! (not a repo, no commits, no git binary) degrade to `None` rather than
//! failing the run.

use std::path::Path;
use std::process::Command;

pub(crate) fn git_short_sha(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Current git branch under `dir`, or `None` for detached HEAD / no repo.
pub(crate) fn current_git_branch(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    // Detached HEAD prints `HEAD`; we'd rather record nothing than a
    // misleading label.
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s)
    }
}
