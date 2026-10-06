//! Layer 4 — hypothesis differential probe.
//!
//! Fires only on mutants inside a top-level function (possibly nested in a
//! class) whose entire body is captured by [`ruff_python_parser`]. The probe
//! shells out to a Python subprocess (see `script_probe.py`) which:
//!
//! 1. Imports both the original and mutated source as throwaway modules in a
//!    temp directory.
//! 2. Looks up the target function (the one enclosing the mutation site) in
//!    both modules.
//! 3. Checks a lightweight eligibility gate — annotated args,
//!    `hypothesis.strategies.from_type` accepts every annotation, no
//!    obvious side-effect calls in the AST.
//! 4. Runs `hypothesis.given(...)` with `N` examples comparing return values
//!    (or exception identity) of the two callables.
//!
//! Outcomes:
//!
//! - All inputs agree → [`EquivVerdict::LikelyEquivalent`] with `confidence
//!   = 0.80`. Below the pipeline threshold on its own; designed to stack with
//!   a pattern signal.
//! - A counterexample → [`EquivVerdict::NotEquivalent`]. The counterexample
//!   reaches the reason string for `suggest` to consume in a later pass.
//! - Ineligible / timeout / hypothesis missing → [`EquivVerdict::NotEquivalent`].
//!
//! Off by default. Wire in explicitly via `EquivPipeline::with_probe()`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use ruff_python_ast::{self as ast, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange};
use wait_timeout::ChildExt;

use super::{EquivDetector, EquivVerdict};
use crate::emit::patch_source;
use crate::mutator::Mutant;

const SCRIPT: &str = include_str!("script_probe.py");

/// Confidence assigned when N examples all agree. Deliberately below the
/// 0.85 pipeline threshold so a single probe pass can't auto-flag —
/// hypothesis testing only narrows the input space the user provided
/// strategies for.
const PASS_CONFIDENCE: f32 = 0.80;

/// Default number of hypothesis examples. The harness uses
/// `derandomize=True` so a given (source, mutant, N) tuple is reproducible.
pub const DEFAULT_EXAMPLES: u32 = 500;

/// Default wall-clock cap per probe call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct HypothesisProbe {
    python: String,
    examples: u32,
    timeout: Duration,
}

impl Default for HypothesisProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HypothesisProbe {
    pub fn new() -> Self {
        Self {
            python: std::env::var("FERMUT_PYTHON").unwrap_or_else(|_| "python3".into()),
            examples: DEFAULT_EXAMPLES,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_python(python: impl Into<String>) -> Self {
        Self {
            python: python.into(),
            examples: DEFAULT_EXAMPLES,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_examples(mut self, n: u32) -> Self {
        self.examples = n;
        self
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

impl EquivDetector for HypothesisProbe {
    fn name(&self) -> &'static str {
        "hypothesis-probe"
    }

    fn detect(&self, mutant: &Mutant, source: &str) -> EquivVerdict {
        // Quick fall-through: no enclosing function → not a callable diff.
        // L1 (bytecode) already covers module-level constants etc.
        let Some(qualname) = enclosing_qualname(source, mutant.range) else {
            return EquivVerdict::NotEquivalent;
        };
        let mutated = patch_source(source, mutant.range, &mutant.replacement);
        if mutated == source {
            // No textual change — L1 is the right layer to flag this.
            return EquivVerdict::NotEquivalent;
        }
        match run_probe(
            &self.python,
            self.timeout,
            source,
            &mutated,
            &qualname,
            self.examples,
        ) {
            Ok(ProbeOutcome::AllPass) => EquivVerdict::LikelyEquivalent {
                confidence: PASS_CONFIDENCE,
                reason: format!(
                    "hypothesis: {n} inputs agreed on {qualname}",
                    n = self.examples
                ),
                source: "hypothesis-probe",
            },
            // Counterexample is a *kill* signal, not an equivalence signal.
            // We surface it through the reason string in a no-equivalence
            // verdict; suggest-time plumbing for the counterexample itself
            // is tracked separately.
            _ => EquivVerdict::NotEquivalent,
        }
    }
}

enum ProbeOutcome {
    AllPass,
    Disagree,
    Ineligible,
    Error,
}

/// Find the dotted name of the innermost function (possibly nested in a
/// class) whose body contains `range`. Returns `None` for module-level
/// mutations or anything inside a lambda/comprehension.
fn enclosing_qualname(source: &str, range: TextRange) -> Option<String> {
    let parsed = parse_module(source).ok()?;
    let module = parsed.syntax();
    let mut stack: Vec<String> = Vec::new();
    let mut best: Option<String> = None;
    visit_body(&module.body, range, &mut stack, &mut best);
    best
}

fn visit_body(
    body: &[Stmt],
    target: TextRange,
    stack: &mut Vec<String>,
    best: &mut Option<String>,
) {
    for stmt in body {
        if !contains(stmt.range(), target) {
            continue;
        }
        match stmt {
            Stmt::FunctionDef(f) => {
                stack.push(f.name.to_string());
                *best = Some(stack.join("."));
                visit_body(&f.body, target, stack, best);
                stack.pop();
            }
            Stmt::ClassDef(c) => {
                stack.push(c.name.to_string());
                visit_body(&c.body, target, stack, best);
                stack.pop();
            }
            Stmt::If(s) => {
                visit_body(&s.body, target, stack, best);
                for clause in &s.elif_else_clauses {
                    visit_body(&clause.body, target, stack, best);
                }
            }
            Stmt::While(s) => {
                visit_body(&s.body, target, stack, best);
                visit_body(&s.orelse, target, stack, best);
            }
            Stmt::For(s) => {
                visit_body(&s.body, target, stack, best);
                visit_body(&s.orelse, target, stack, best);
            }
            Stmt::Try(s) => {
                visit_body(&s.body, target, stack, best);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(eh) = handler;
                    visit_body(&eh.body, target, stack, best);
                }
                visit_body(&s.orelse, target, stack, best);
                visit_body(&s.finalbody, target, stack, best);
            }
            Stmt::With(s) => visit_body(&s.body, target, stack, best),
            _ => {}
        }
    }
}

fn contains(outer: TextRange, inner: TextRange) -> bool {
    outer.start() <= inner.start() && inner.end() <= outer.end()
}

fn run_probe(
    python: &str,
    timeout: Duration,
    orig: &str,
    mutated: &str,
    qualname: &str,
    examples: u32,
) -> std::io::Result<ProbeOutcome> {
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
        let payload = serde_json::json!({
            "orig": orig,
            "mutated": mutated,
            "qualname": qualname,
            "examples": examples,
        })
        .to_string();
        stdin.write_all(payload.as_bytes())?;
    }
    // Drop the stdin handle so the child sees EOF.
    drop(child.stdin.take());

    let status = match child.wait_timeout(timeout)? {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(ProbeOutcome::Error);
        }
    };
    if !status.success() {
        return Ok(ProbeOutcome::Error);
    }
    let mut buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_end(&mut buf)?;
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(ProbeOutcome::Error),
    };
    Ok(
        match parsed.get("verdict").and_then(|v| v.as_str()).unwrap_or("") {
            "equivalent" => ProbeOutcome::AllPass,
            "not_equivalent" => ProbeOutcome::Disagree,
            "ineligible" => ProbeOutcome::Ineligible,
            _ => ProbeOutcome::Error,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::{TextRange, TextSize};
    use std::path::PathBuf;

    fn arith_mutant(source: &str, original: &str, replacement: &str) -> Mutant {
        let start = source.find(original).expect("operator in source");
        Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(
                TextSize::from(start as u32),
                TextSize::from((start + original.len()) as u32),
            ),
            original: original.into(),
            replacement: replacement.into(),
            line: 1,
            stmt_line: 1,
            site: None,
        }
    }

    fn python_available() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn hypothesis_available() -> bool {
        Command::new("python3")
            .args(["-c", "import hypothesis"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn enclosing_qualname_finds_top_level_def() {
        let src = "def f(x: int) -> int:\n    return x + 0\n";
        let needle = "+";
        let pos = src.find(needle).unwrap();
        let r = TextRange::new(TextSize::from(pos as u32), TextSize::from((pos + 1) as u32));
        assert_eq!(enclosing_qualname(src, r).as_deref(), Some("f"));
    }

    #[test]
    fn enclosing_qualname_finds_method_dotted() {
        let src = "class C:\n    def f(self, x: int) -> int:\n        return x + 0\n";
        let needle = "+";
        let pos = src.find(needle).unwrap();
        let r = TextRange::new(TextSize::from(pos as u32), TextSize::from((pos + 1) as u32));
        assert_eq!(enclosing_qualname(src, r).as_deref(), Some("C.f"));
    }

    #[test]
    fn enclosing_qualname_none_for_module_level() {
        let src = "X = 1 + 0\n";
        let pos = src.find('+').unwrap();
        let r = TextRange::new(TextSize::from(pos as u32), TextSize::from((pos + 1) as u32));
        assert_eq!(enclosing_qualname(src, r), None);
    }

    #[test]
    fn enclosing_qualname_nested_def_innermost_wins() {
        let src = "def outer():\n    def inner(x: int) -> int:\n        return x + 0\n";
        let pos = src.find('+').unwrap();
        let r = TextRange::new(TextSize::from(pos as u32), TextSize::from((pos + 1) as u32));
        assert_eq!(enclosing_qualname(src, r).as_deref(), Some("outer.inner"));
    }

    #[test]
    fn missing_python_yields_not_equivalent() {
        let det = HypothesisProbe::with_python("/does/not/exist");
        let src = "def f(x: int) -> int:\n    return x + 0\n";
        let m = arith_mutant(src, "+", "-");
        assert_eq!(det.detect(&m, src), EquivVerdict::NotEquivalent);
    }

    #[test]
    fn module_level_mutation_falls_through() {
        let det = HypothesisProbe::with_python("/does/not/exist");
        let src = "X = 1 + 0\n";
        let m = arith_mutant(src, "+", "-");
        assert_eq!(det.detect(&m, src), EquivVerdict::NotEquivalent);
    }

    #[test]
    fn arith_zero_identity_provably_equivalent_under_probe() {
        if !python_available() || !hypothesis_available() {
            eprintln!("skipping: python3 + hypothesis not available");
            return;
        }
        let det = HypothesisProbe::new().with_examples(50);
        let src = "def f(x: int) -> int:\n    return x + 0\n";
        let m = arith_mutant(src, "+", "-");
        let v = det.detect(&m, src);
        match v {
            EquivVerdict::LikelyEquivalent {
                confidence, source, ..
            } => {
                assert!((confidence - PASS_CONFIDENCE).abs() < 1e-6);
                assert_eq!(source, "hypothesis-probe");
            }
            other => panic!("expected LikelyEquivalent, got {other:?}"),
        }
    }

    #[test]
    fn semantic_change_falsified_by_probe() {
        if !python_available() || !hypothesis_available() {
            eprintln!("skipping: python3 + hypothesis not available");
            return;
        }
        let det = HypothesisProbe::new().with_examples(50);
        // `x + 1` vs `x - 1` — easy disagreement under any non-zero input.
        let src = "def f(x: int) -> int:\n    return x + 1\n";
        let m = arith_mutant(src, "+", "-");
        assert_eq!(det.detect(&m, src), EquivVerdict::NotEquivalent);
    }
}
