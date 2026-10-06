//! Trivial Compiler Equivalence (TCE) pre-filter.
//!
//! `compile()` the original file and the mutant, then compare the recursive
//! code-object signatures. A byte-identical result is **proof** the mutant
//! cannot differ from the original at runtime (CPython folded both AST shapes
//! to the same VM instructions and constants), so the mutant is dropped before
//! it ever reaches the test runner — no test cost at all.
//!
//! Catches a class of useless mutants for free:
//!
//! - `return` ↔ `return None` when the function already returned `None`,
//! - constant-folded arithmetic (`x * 1`, `x + 0` that the peephole optimizer
//!   collapses),
//! - whitespace- / paren-only diffs that survive the source patch.
//!
//! This reuses [`crate::equiv::bytecode::BytecodeIdentity`] — the same
//! compile-and-compare the post-run equivalence detector uses — but on the
//! *pre*-run side of the pipeline. There it reclassifies a survivor as
//! `Equivalent` (visible, with a reason); here it silently `Skipped`s the
//! mutant so the test is never spent. Both are sound: a byte-identical signature
//! is a proof, not a heuristic.
//!
//! Fails **open**: a missing interpreter, unreadable file, or malformed script
//! result admits the mutant (returns `Ok(true)`), matching the filter chain's
//! contract — a filter must never drop a mutant it cannot prove equivalent.
//! `FERMUT_PYTHON` overrides the interpreter (default `python3`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::Filter;
use crate::equiv::bytecode::BytecodeIdentity;
use crate::equiv::{EquivDetector, EquivVerdict};
use crate::mutator::Mutant;
use crate::sync::lock_recover;

pub struct TceFilter {
    detector: BytecodeIdentity,
    /// Original (un-patched) source per file, read lazily and memoized. Every
    /// mutant in a file compiles against the same original, so we read it once.
    sources: Mutex<HashMap<PathBuf, Arc<String>>>,
}

impl Default for TceFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl TceFilter {
    pub fn new() -> Self {
        Self {
            detector: BytecodeIdentity::new(),
            sources: Mutex::new(HashMap::new()),
        }
    }

    fn source_for(&self, file: &PathBuf) -> Result<Arc<String>> {
        if let Some(src) = lock_recover(&self.sources).get(file) {
            return Ok(Arc::clone(src));
        }
        let src = Arc::new(std::fs::read_to_string(file)?);
        lock_recover(&self.sources).insert(file.clone(), Arc::clone(&src));
        Ok(src)
    }
}

impl Filter for TceFilter {
    fn name(&self) -> &'static str {
        "tce"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        // Fail open on an unreadable source: a filter must never drop a mutant
        // it cannot prove equivalent, and this keeps the two call sites
        // consistent — the engine's per-mutant loop maps `Err` to admit, but
        // the `list` subcommand's `first_rejector` propagates it and would
        // otherwise abort the whole listing.
        let source = match self.source_for(&mutant.file) {
            Ok(src) => src,
            Err(_) => return Ok(true),
        };
        // Drop only on *proof*. `BytecodeIdentity` returns either
        // `ProvablyEquivalent` or `NotEquivalent` (never a `Likely`), and it
        // already fails open to `NotEquivalent` on a missing/broken interpreter,
        // so a drop here is always backed by identical code-object signatures.
        match self.detector.detect(mutant, &source) {
            EquivVerdict::ProvablyEquivalent { .. } => Ok(false),
            _ => Ok(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::{TextRange, TextSize};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn python_available() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Write `source` to a temp `.py` file and build a mutant that replaces the
    /// first occurrence of `original` with `replacement`.
    fn mutant_in_file(
        source: &str,
        original: &str,
        replacement: &str,
    ) -> (tempfile::NamedTempFile, Mutant) {
        let tmp = tempfile::Builder::new()
            .prefix("fermut-tce-")
            .suffix(".py")
            .tempfile()
            .unwrap();
        tmp.as_file().write_all(source.as_bytes()).unwrap();
        let start = source.find(original).unwrap();
        let m = Mutant {
            id: "t".into(),
            file: tmp.path().to_path_buf(),
            operator: Operator::ReturnValueToNone,
            range: TextRange::new(
                TextSize::from(start as u32),
                TextSize::from((start + original.len()) as u32),
            ),
            original: original.into(),
            replacement: replacement.into(),
            line: 1,
            stmt_line: 1,
            site: None,
        };
        (tmp, m)
    }

    #[test]
    fn equivalent_mutant_is_dropped() {
        if !python_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let f = TceFilter::new();
        // `return None` → `return`: bytecode-identical, so drop it.
        let (_tmp, m) = mutant_in_file("def f():\n    return None\n", "return None", "return");
        assert!(!f.admits(&m).unwrap(), "equivalent mutant must be dropped");
    }

    #[test]
    fn semantic_mutant_is_admitted() {
        if !python_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let f = TceFilter::new();
        // `1` → `2`: real behavior change, so admit it for testing.
        let (_tmp, m) = mutant_in_file("def f():\n    return 1\n", "1", "2");
        assert!(f.admits(&m).unwrap(), "semantic mutant must be admitted");
    }

    #[test]
    fn unreadable_file_fails_open() {
        // A path that does not exist makes `source_for` error; the filter
        // fails open — admits the mutant (`Ok(true)`) rather than surfacing an
        // `Err` — so both call sites (engine loop, `list`'s `first_rejector`)
        // behave the same and no mutant is dropped without proof.
        let f = TceFilter::new();
        let m = Mutant {
            id: "t".into(),
            file: PathBuf::from("/does/not/exist.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
            site: None,
        };
        assert!(f.admits(&m).unwrap(), "unreadable file must fail open");
    }

    #[test]
    fn source_is_read_once_per_file() {
        if !python_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let f = TceFilter::new();
        let (_tmp, m) = mutant_in_file("def f():\n    return 1\n", "1", "2");
        f.admits(&m).unwrap();
        // Second call must hit the memoized source rather than re-reading.
        f.admits(&m).unwrap();
        assert_eq!(lock_recover(&f.sources).len(), 1);
    }
}
