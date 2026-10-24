//! ruff lint pre-filter.
//!
//! Per mutant: write the patched source to a temp file, run `ruff check
//! --output-format=concise` against it, count diagnostics. If the mutant
//! introduces *more* diagnostics than the original file had, drop it.
//!
//! Cheaper than ty (no type inference), broader than ty (catches dead-code,
//! useless-comparisons, F-series unused-name lints). Useful as an extra
//! pre-filter in front of ty.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use tracing::warn;

use super::Filter;
use crate::emit::patch_source;
use crate::mutator::Mutant;
use crate::runner::resolve_tool;
use crate::sync::lock_recover;

pub struct RuffFilter {
    bin: PathBuf,
    baseline: Mutex<HashMap<PathBuf, usize>>,
}

impl RuffFilter {
    /// Resolve `ruff` preferring the project's venv `bin/` (derived from
    /// `source_root`), then the ambient PATH — the same resolution `doctor`
    /// reports, so a green `ruff ok` there means the run finds it too.
    pub fn new(source_root: &Path) -> Result<Self> {
        let bin = resolve_tool("ruff", source_root).ok_or_else(|| {
            anyhow!(
                "ruff binary not found in the project venv or on PATH; install via \
                 `uv tool install ruff` (or `pipx install ruff`), or omit the --ruff-filter flag"
            )
        })?;
        Ok(Self {
            bin,
            baseline: Mutex::new(HashMap::new()),
        })
    }

    fn baseline_count_for(&self, file: &PathBuf) -> Result<Option<usize>> {
        if let Some(&n) = lock_recover(&self.baseline).get(file) {
            return Ok(Some(n));
        }
        match self.diagnostic_count(file.to_string_lossy().as_ref())? {
            Some(n) => {
                lock_recover(&self.baseline).insert(file.clone(), n);
                Ok(Some(n))
            }
            None => Ok(None),
        }
    }

    /// `Ok(None)` when the `ruff` subprocess outran the filter timeout — the
    /// caller bypasses the filter (admits the mutant) rather than hanging.
    fn diagnostic_count(&self, path: &str) -> Result<Option<usize>> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["check", "--no-fix", "--output-format", "concise", path]);
        let Some(out) = super::run_filter_with_timeout(cmd)
            .with_context(|| format!("invoking `{} check {}`", self.bin.display(), path))?
        else {
            warn!(
                path,
                timeout_secs = super::FILTER_SUBPROCESS_TIMEOUT.as_secs(),
                "ruff check timed out; bypassing ruff filter for this file"
            );
            return Ok(None);
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Concise lines look like `path:line:col: RULE message`. Each line
        // is one diagnostic; ignore footer like `Found N error(s).`.
        Ok(Some(
            stdout.lines().filter(|l| looks_like_diagnostic(l)).count(),
        ))
    }
}

impl Filter for RuffFilter {
    fn name(&self) -> &'static str {
        "ruff"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        let Some(baseline) = self.baseline_count_for(&mutant.file)? else {
            return Ok(true); // ruff timed out on baseline → bypass
        };
        let original = std::fs::read_to_string(&mutant.file)
            .with_context(|| format!("reading {}", mutant.file.display()))?;
        let patched = patch_source(&original, mutant.range, &mutant.replacement);

        let tmp = super::patched_tempfile(&mutant.file, &patched)?;
        let Some(mutated) = self.diagnostic_count(tmp.path().to_string_lossy().as_ref())? else {
            return Ok(true); // ruff timed out on mutant → bypass
        };
        Ok(mutated <= baseline)
    }
}

fn looks_like_diagnostic(line: &str) -> bool {
    // `path:line:col: CODE message` — find a `:<digits>:<digits>:` run after
    // the path. Scanning for it (rather than counting leading colons) holds for
    // any path length and for Windows drive letters (`C:\\…`).
    let is_num = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = line.split(':').skip(1).collect();
    parts.windows(3).any(|w| is_num(w[0]) && is_num(w[1]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::{TextRange, TextSize};

    #[test]
    fn diagnostic_lines_match_any_path_length() {
        let long = format!("/{}/x.py:3:1: F401 unused", "d".repeat(60));
        assert!(looks_like_diagnostic(&long));
        assert!(looks_like_diagnostic(
            "C:\\p\\x.py:3:12: E1 msg: with colon"
        ));
        assert!(!looks_like_diagnostic("Found 3 errors."));
        assert!(!looks_like_diagnostic("x.py:3: no column"));
        assert!(!looks_like_diagnostic("x.py:a:1: bad line"));
        assert!(!looks_like_diagnostic(""));
    }

    /// A fake `ruff` reporting one diagnostic per line containing `BAD`, with
    /// the (long, absolute) path ruff was given.
    #[cfg(unix)]
    fn fake_ruff(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("ruff");
        std::fs::write(
            &bin,
            "#!/bin/sh\nawk -v f=\"$5\" '/BAD/{print f \":1:1: F821 undefined\"}' \"$5\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn mutant(file: &Path, replacement: &str) -> Mutant {
        Mutant {
            id: "t".into(),
            file: file.to_path_buf(),
            operator: Operator::NumberShift,
            range: TextRange::new(TextSize::from(4), TextSize::from(5)),
            original: "1".into(),
            replacement: replacement.into(),
            line: 1,
            stmt_line: 1,
            site: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn admits_only_mutants_that_add_no_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a_rather_long_module_name_for_paths.py");
        std::fs::write(&src, "x = 1\ny = BAD\n").unwrap();
        let f = RuffFilter {
            bin: fake_ruff(dir.path()),
            baseline: Mutex::new(HashMap::new()),
        };
        assert_eq!(f.name(), "ruff");
        assert!(f.admits(&mutant(&src, "2")).unwrap());
        assert_eq!(lock_recover(&f.baseline).get(&src), Some(&1));
        assert!(!f.admits(&mutant(&src, "BAD")).unwrap());
    }

    #[test]
    fn missing_source_is_an_error() {
        let f = RuffFilter {
            bin: PathBuf::from("true"),
            baseline: Mutex::new(HashMap::new()),
        };
        // `true` prints nothing: baseline 0, then reading the source fails.
        assert!(f
            .admits(&mutant(Path::new("/nonexistent/x.py"), "2"))
            .is_err());
    }
}
