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

use super::Filter;
use crate::emit::patch_source;
use crate::mutator::Mutant;
use crate::sync::lock_recover;

pub struct RuffFilter {
    bin: String,
    baseline: Mutex<HashMap<PathBuf, usize>>,
}

impl RuffFilter {
    pub fn new() -> Result<Self> {
        let bin = which("ruff").context(
            "ruff binary not found; install via `pipx install ruff` or `uv tool install ruff`, or omit the --ruff-filter flag",
        )?;
        Ok(Self {
            bin,
            baseline: Mutex::new(HashMap::new()),
        })
    }

    fn baseline_count_for(&self, file: &PathBuf) -> Result<usize> {
        if let Some(&n) = lock_recover(&self.baseline).get(file) {
            return Ok(n);
        }
        let n = self.diagnostic_count(file.to_string_lossy().as_ref())?;
        lock_recover(&self.baseline).insert(file.clone(), n);
        Ok(n)
    }

    fn diagnostic_count(&self, path: &str) -> Result<usize> {
        let out = Command::new(&self.bin)
            .args(["check", "--no-fix", "--output-format", "concise", path])
            .output()
            .with_context(|| format!("invoking `{} check {}`", self.bin, path))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Concise lines look like `path:line:col: RULE message`. Each line
        // is one diagnostic; ignore footer like `Found N error(s).`.
        Ok(stdout.lines().filter(|l| looks_like_diagnostic(l)).count())
    }
}

impl Filter for RuffFilter {
    fn name(&self) -> &'static str {
        "ruff"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        let baseline = self.baseline_count_for(&mutant.file)?;
        let original = std::fs::read_to_string(&mutant.file)
            .with_context(|| format!("reading {}", mutant.file.display()))?;
        let patched = patch_source(&original, mutant.range, &mutant.replacement);

        // Temp file goes in the original's directory, not /tmp, so ruff picks
        // up the same nearest `[tool.ruff]` config and per-file ignores the
        // baseline saw. A /tmp file would resolve a different (or no) config,
        // skewing the diagnostic count asymmetrically.
        let parent = mutant.file.parent().unwrap_or_else(|| Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix("fermut-ruff-")
            .suffix(".py")
            .tempfile_in(parent)
            .context("creating temp file")?;
        std::fs::write(tmp.path(), &patched).context("writing patched source")?;

        let mutated = self.diagnostic_count(tmp.path().to_string_lossy().as_ref())?;
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

fn which(bin: &str) -> Result<String> {
    let out = Command::new("which").arg(bin).output();
    if let Ok(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return Ok(s);
            }
        }
    }
    Err(anyhow!("`{bin}` not on PATH"))
}
