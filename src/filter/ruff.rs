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
    // path:line:col: CODE message  — count colons at start.
    let mut colons = 0;
    for c in line.chars().take(40) {
        if c == ':' {
            colons += 1;
            if colons >= 3 {
                return true;
            }
        }
    }
    false
}
