//! Diff-based filters.
//!
//! Two modes:
//!
//! - **diff-only** (`--diff-only <base>`) — branch-relative. Diffs from the
//!   merge-base of `<base>` and `HEAD` to the working tree: lines changed on
//!   the current branch since it diverged from `<base>`, *including*
//!   uncommitted edits. (Anchoring at the merge-base is what a three-dot
//!   `<base>...HEAD` gives, but three-dot is commit-to-commit only and would
//!   silently ignore working-tree changes — so we resolve the merge-base and
//!   run a plain two-dot `git diff <merge-base>`.)
//! - **since** (`--since <spec>`) — point-relative. `<spec>` is resolved as
//!   either a git ref (commit SHA / tag / branch / `HEAD~5` / etc.) or a date
//!   string (`2025-12-01`, `'1 week ago'`). Diff includes the working tree:
//!   any line touched since that point, including uncommitted edits.
//!
//! Both populate the same `path -> line-set` map and use the same `admits`
//! check, so they live behind one filter type.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{anyhow, Context, Result};

use super::Filter;
use crate::mutator::Mutant;

pub struct DiffFilter {
    changed: HashMap<PathBuf, HashSet<u32>>,
    label: &'static str,
}

impl DiffFilter {
    /// Branch-relative diff: lines changed on this branch since `base`,
    /// including uncommitted working-tree edits. Anchored at the merge-base of
    /// `base` and `HEAD` (branch-divergence point), then diffed two-dot against
    /// the working tree so current, unstaged work is scoped in.
    pub fn from_git(base: &str, cwd: &Path) -> Result<Self> {
        let merge_base = git_merge_base(base, cwd)?;
        let stdout = git_diff(&["--unified=0", "--relative", &merge_base], cwd)?;
        Ok(Self {
            changed: parse_unified_diff(&stdout, cwd),
            label: "diff-only",
        })
    }

    /// Point-relative diff: lines touched since `spec` (commit ref or date),
    /// including uncommitted working-tree edits.
    pub fn from_git_since(spec: &str, cwd: &Path) -> Result<Self> {
        let commit = resolve_since_spec(spec, cwd)?;
        let stdout = git_diff(&["--unified=0", "--relative", &commit], cwd)?;
        Ok(Self {
            changed: parse_unified_diff(&stdout, cwd),
            label: "since",
        })
    }
}

impl Filter for DiffFilter {
    fn name(&self) -> &'static str {
        self.label
    }

    fn admits(&self, m: &Mutant) -> Result<bool> {
        let canonical = m.file.canonicalize().unwrap_or_else(|_| m.file.clone());
        Ok(self
            .changed
            .get(&canonical)
            .map(|set| set.contains(&m.line))
            .unwrap_or(false))
    }
}

/// Resolve the merge-base (branch-divergence point) of `base` and `HEAD`.
/// Reuses `git_diff_error` for a friendly message on the common failures
/// (outside a repo, base ref not resolvable).
fn git_merge_base(base: &str, cwd: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["merge-base", base, "HEAD"])
        .current_dir(cwd)
        .output()
        .context("could not run `git` — is it installed and on PATH?")?;
    if !out.status.success() {
        return Err(git_diff_error(
            &String::from_utf8_lossy(&out.stderr),
            out.status,
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err(anyhow!(
            "git merge-base of {base:?} and HEAD is empty — unrelated histories?"
        ));
    }
    Ok(s)
}

fn git_diff(args: &[&str], cwd: &Path) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("diff").args(args).current_dir(cwd);
    let out = cmd
        .output()
        .context("could not run `git` — is it installed and on PATH?")?;
    if !out.status.success() {
        return Err(git_diff_error(
            &String::from_utf8_lossy(&out.stderr),
            out.status,
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Turn a failed `git diff` into one actionable line plus a fix hint, instead
/// of echoing git's full (often multi-line) stderr. The two common causes when
/// diff scoping is enabled — typically via `fermut init --profile pr-gate`,
/// which sets `diff_only = "main"` — are running outside a git repo and a base
/// ref that doesn't exist locally. Both point the user at `--no-diff-only`.
fn git_diff_error(stderr: &str, status: ExitStatus) -> anyhow::Error {
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)");
    let lower = stderr.to_ascii_lowercase();
    let hint = if lower.contains("not a git repository") {
        "diff scoping needs a git repository. Run fermut from inside a git \
         checkout, or turn it off: pass `--no-diff-only` on the CLI, or remove \
         the `diff_only`/`since` key from fermut.toml."
    } else if lower.contains("unknown revision")
        || lower.contains("ambiguous argument")
        || lower.contains("bad revision")
    {
        "the diff base could not be resolved. Point `diff_only`/`--diff-only` \
         at a branch or commit that exists locally (e.g. `origin/main`), fetch \
         it first, or turn diff scoping off with `--no-diff-only`."
    } else {
        "fix the git error above, or turn diff scoping off with `--no-diff-only`."
    };
    anyhow!("git diff failed ({status}): {first}\n\nhint: {hint}")
}

/// Resolve `spec` to a commit SHA. Tries `git rev-parse --verify` first
/// (handles refs, tags, partial SHAs, `HEAD~5`, etc.). If that fails, treats
/// `spec` as a date and asks `git log -1 --before=<spec>` for the latest
/// commit on `HEAD` predating it.
fn resolve_since_spec(spec: &str, cwd: &Path) -> Result<String> {
    let rev = Command::new("git")
        .args(["rev-parse", "--verify", &format!("{spec}^{{commit}}")])
        .current_dir(cwd)
        .output()
        .context("invoking git rev-parse")?;
    if rev.status.success() {
        let s = String::from_utf8_lossy(&rev.stdout).trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }

    let log = Command::new("git")
        .args([
            "log",
            "-1",
            &format!("--before={spec}"),
            "--format=%H",
            "HEAD",
        ])
        .current_dir(cwd)
        .output()
        .context("invoking git log")?;
    if log.status.success() {
        let s = String::from_utf8_lossy(&log.stdout).trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }

    Err(anyhow!(
        "could not resolve --since spec {spec:?} as a git ref or a date (no commits before it on HEAD)"
    ))
}

fn parse_unified_diff(diff: &str, cwd: &Path) -> HashMap<PathBuf, HashSet<u32>> {
    let mut changed: HashMap<PathBuf, HashSet<u32>> = HashMap::new();
    let mut current: Option<PathBuf> = None;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            let p = cwd.join(path);
            current = Some(p.canonicalize().unwrap_or(p));
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current = None;
            continue;
        }
        if let Some((start, len)) = parse_hunk_header(line) {
            if let Some(p) = &current {
                let entry = changed.entry(p.clone()).or_default();
                for ln in start..start + len {
                    entry.insert(ln);
                }
            }
        }
    }
    changed
}

/// Parse a unified-diff hunk header like `@@ -A,B +C,D @@` and return the
/// new-side (start, len). `len` defaults to 1 when omitted. Hunks where
/// `len == 0` (pure deletion) yield (start, 0) — no new lines.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let after_minus = line.strip_prefix("@@ -")?;
    let plus_idx = after_minus.find(" +")?;
    let plus_part = &after_minus[plus_idx + 2..];
    let end = plus_part.find(" @@")?;
    let new_part = &plus_part[..end];
    let mut parts = new_part.splitn(2, ',');
    let start: u32 = parts.next()?.parse().ok()?;
    let len: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    Some((start, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_with_explicit_len() {
        assert_eq!(parse_hunk_header("@@ -10,3 +20,5 @@"), Some((20, 5)));
    }

    /// A non-zero `ExitStatus` to feed the error formatter, obtained portably
    /// by running git with a bogus subcommand (git is a fermut prerequisite).
    fn failing_status() -> ExitStatus {
        Command::new("git")
            .arg("not-a-real-subcommand-xyz")
            .output()
            .expect("git on PATH")
            .status
    }

    #[test]
    fn git_diff_error_shows_only_first_line_not_full_dump() {
        let noisy = "fatal: not a git repository (or any of the parent directories): .git\nusage: git diff ...\n<many more lines>\n";
        let msg = git_diff_error(noisy, failing_status()).to_string();
        assert!(msg.contains("fatal: not a git repository"));
        // The verbose usage dump must be dropped — only the first line survives.
        assert!(!msg.contains("<many more lines>"));
        assert!(!msg.contains("usage: git diff"));
    }

    #[test]
    fn git_diff_error_hints_no_repo() {
        let msg = git_diff_error(
            "fatal: not a git repository (or any of the parent directories): .git",
            failing_status(),
        )
        .to_string();
        assert!(msg.contains("--no-diff-only"));
        assert!(msg.contains("git repository"));
    }

    #[test]
    fn git_diff_error_hints_unknown_base() {
        let msg = git_diff_error(
            "fatal: ambiguous argument 'main...HEAD': unknown revision or path not in the working tree.",
            failing_status(),
        )
        .to_string();
        assert!(msg.contains("--no-diff-only"));
        assert!(msg.contains("diff base"));
    }

    #[test]
    fn hunk_with_omitted_len() {
        assert_eq!(parse_hunk_header("@@ -10 +20 @@"), Some((20, 1)));
    }

    #[test]
    fn hunk_with_trailing_context() {
        assert_eq!(
            parse_hunk_header("@@ -10,3 +20,5 @@ def foo():"),
            Some((20, 5))
        );
    }

    fn git(cwd: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git on PATH")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// Regression: `--diff-only` must scope in uncommitted working-tree edits
    /// on the branch, matching `--since`. The old three-dot `base...HEAD` was
    /// commit-to-commit only and silently dropped unstaged work, filtering out
    /// every mutant. Anchors at the merge-base but diffs against the tree.
    #[test]
    fn diff_only_includes_uncommitted_branch_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        git(cwd, &["init", "-q", "-b", "main"]);
        git(cwd, &["config", "user.email", "t@t"]);
        git(cwd, &["config", "user.name", "t"]);
        std::fs::write(cwd.join("f.py"), "a\nb\nc\n").unwrap();
        git(cwd, &["add", "."]);
        git(cwd, &["commit", "-qm", "base"]);
        git(cwd, &["checkout", "-qb", "feat"]);
        // Uncommitted edit to line 2 on the branch.
        std::fs::write(cwd.join("f.py"), "a\nB\nc\n").unwrap();

        let filter = DiffFilter::from_git("main", cwd).unwrap();
        let key = cwd.join("f.py");
        let key = key.canonicalize().unwrap_or(key);
        let set = filter.changed.get(&key).cloned().unwrap_or_default();
        assert_eq!(
            set,
            [2u32].into_iter().collect(),
            "diff-only should scope in the uncommitted edit to line 2"
        );
    }

    #[test]
    fn parse_sets_changed_lines() {
        let diff = "\
diff --git a/foo.py b/foo.py
--- a/foo.py
+++ b/foo.py
@@ -1,0 +2,3 @@
+a
+b
+c
";
        let cwd = std::env::current_dir().unwrap();
        let changed = parse_unified_diff(diff, &cwd);
        let key = cwd.join("foo.py");
        let key = key.canonicalize().unwrap_or(key);
        let set = changed.get(&key).cloned().unwrap_or_default();
        assert_eq!(set, [2u32, 3, 4].into_iter().collect());
    }
}
