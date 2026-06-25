//! Regenerate a unified diff of the mutated file vs. the working-copy source.
//!
//! Reads the original file from disk on demand. If the file has changed since
//! the run, the diff may not match what was tested — the caller should warn
//! the user.

use anyhow::{Context, Result};
use similar::TextDiff;

use crate::emit::patch_source;
use crate::mutator::Mutant;

pub fn unified_diff_for(mutant: &Mutant) -> Result<String> {
    let original = std::fs::read_to_string(&mutant.file)
        .with_context(|| format!("reading {}", mutant.file.display()))?;
    let patched = patch_source(&original, mutant.range, &mutant.replacement);
    let diff = TextDiff::from_lines(&original, &patched);
    let header_left = format!("{} (original)", mutant.file.display());
    let header_right = format!("{} (mutant)", mutant.file.display());
    Ok(diff
        .unified_diff()
        .context_radius(3)
        .header(&header_left, &header_right)
        .to_string())
}
