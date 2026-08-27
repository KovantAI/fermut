//! Layer 1 — bytecode identity via CPython.
//!
//! Compiles original + mutated source with `compile()` and compares
//! recursive code-object signatures. A match is **proof** of behavioral
//! equivalence: CPython collapsed the two AST shapes to the same VM
//! instructions and constants.
//!
//! Catches:
//!
//! - `return` ↔ `return None` (the `return-value-to-none` operator when the
//!   function already returned `None`).
//! - Whitespace- / paren-only diffs that survive the source patch.
//! - Any future operator whose mutation reduces to the original under
//!   constant folding or simple peephole optimization.
//!
//! Fails open: a missing Python interpreter or a malformed script result
//! yields [`EquivVerdict::NotEquivalent`] rather than poisoning the run.
//! `FERMUT_PYTHON` overrides the interpreter path (default `python3`).

use std::io::Write;
use std::process::{Command, Stdio};

use super::{EquivDetector, EquivVerdict};
use crate::emit::patch_source;
use crate::mutator::Mutant;

const SCRIPT: &str = include_str!("script.py");

pub struct BytecodeIdentity {
    python: String,
}

impl Default for BytecodeIdentity {
    fn default() -> Self {
        Self::new()
    }
}

impl BytecodeIdentity {
    pub fn new() -> Self {
        Self {
            python: std::env::var("FERMUT_PYTHON").unwrap_or_else(|_| "python3".into()),
        }
    }

    pub fn with_python(python: impl Into<String>) -> Self {
        Self {
            python: python.into(),
        }
    }
}

impl EquivDetector for BytecodeIdentity {
    fn name(&self) -> &'static str {
        "bytecode-identity"
    }

    fn detect(&self, mutant: &Mutant, source: &str) -> EquivVerdict {
        let mutated = patch_source(source, mutant.range, &mutant.replacement);
        if mutated == source {
            return EquivVerdict::ProvablyEquivalent {
                reason: "patch is a no-op at the byte level".into(),
                source: "bytecode-identity",
            };
        }
        match run_check(&self.python, source, &mutated) {
            Ok(true) => EquivVerdict::ProvablyEquivalent {
                reason: "compile() produces identical code-object signature".into(),
                source: "bytecode-identity",
            },
            Ok(false) => EquivVerdict::NotEquivalent,
            Err(_) => EquivVerdict::NotEquivalent,
        }
    }
}

fn run_check(python: &str, orig: &str, mutated: &str) -> std::io::Result<bool> {
    let mut child = Command::new(python)
        .arg("-c")
        .arg(SCRIPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing stdin"))?;
        let payload = serde_json::json!({"orig": orig, "mutated": mutated}).to_string();
        stdin.write_all(payload.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Ok(false);
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(parsed
        .get("equivalent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::{TextRange, TextSize};
    use std::path::PathBuf;

    fn python_available() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn mutant_at(source: &str, original: &str, replacement: &str) -> Mutant {
        let start = source.find(original).unwrap();
        Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ReturnValueToNone,
            range: TextRange::new(
                TextSize::from(start as u32),
                TextSize::from((start + original.len()) as u32),
            ),
            original: original.into(),
            replacement: replacement.into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn noop_patch_is_proved_equivalent_without_python() {
        // Replacement equals original → mutated source == source. The fast
        // path returns ProvablyEquivalent before spawning Python at all, so
        // this test runs even without an interpreter.
        let det = BytecodeIdentity::with_python("/does/not/exist");
        let source = "def f():\n    return 1\n";
        let m = mutant_at(source, "1", "1");
        let v = det.detect(&m, source);
        assert!(matches!(v, EquivVerdict::ProvablyEquivalent { .. }));
    }

    #[test]
    fn missing_python_fails_closed_to_not_equivalent() {
        let det = BytecodeIdentity::with_python("/does/not/exist");
        let source = "def f():\n    return 1\n";
        let m = mutant_at(source, "1", "2");
        assert_eq!(det.detect(&m, source), EquivVerdict::NotEquivalent);
    }

    #[test]
    fn return_none_vs_bare_return_is_bytecode_identical() {
        if !python_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let det = BytecodeIdentity::new();
        let source = "def f():\n    return None\n";
        // `return None` → `return` (drop the literal).
        let m = mutant_at(source, "return None", "return");
        let v = det.detect(&m, source);
        assert!(
            matches!(v, EquivVerdict::ProvablyEquivalent { .. }),
            "expected ProvablyEquivalent, got {v:?}"
        );
    }

    #[test]
    fn semantic_change_not_flagged() {
        if !python_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let det = BytecodeIdentity::new();
        let source = "def f():\n    return 1\n";
        let m = mutant_at(source, "1", "2");
        let v = det.detect(&m, source);
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }
}
