//! Prompt builders. Pure functions over [`Mutant`] + project context.
//!
//! Two prompt shapes:
//!
//! - `build_suggest_prompt` — asks the model for a complete pytest
//!   function that kills the mutation. Output must be a single python
//!   code block.
//! - `build_explain_prompt` — asks for prose: why the mutant likely
//!   survived, plus the pytest function. Format: two markdown sections.
//!
//! Both layer the same project context: surrounding source, enclosing
//! symbol, optional style-mimic samples from the existing test tree.

use std::path::Path;

use super::client::LlmRequest;
use super::redact::redact;
use crate::mutator::Mutant;

const SUGGEST_SYSTEM: &str =
    "You are a pytest expert helping a developer kill a surviving mutant from \
the fermut mutation-testing tool. Produce exactly one pytest test function — \
no commentary, no preamble, no markdown headings — wrapped in a single ```python``` \
code block. The test must fail when the mutation is applied and pass otherwise. \
Match the existing test file's import style and conventions when sample tests \
are provided. Use only the standard library and pytest; do not invent helpers. \
If you cannot write a useful test from the given context, return a comment-only \
function explaining what's missing.";

const EXPLAIN_SYSTEM: &str =
    "You are a pytest expert explaining a surviving mutant from the fermut \
mutation-testing tool. Respond in two markdown sections, in this exact order:\n\
\n\
## Why it survived\n\
Two to four sentences. Be concrete about which assertion is missing.\n\
\n\
## Killing test\n\
A single ```python``` code block containing one pytest function that would \
fail under the mutation and pass otherwise. No prose inside the code block.";

/// Inputs the prompt builders consume. Owned strings so callers can drop
/// the source paths after construction.
#[derive(Debug, Clone)]
pub struct PromptContext {
    pub source_snippet: String,
    pub enclosing_symbol: Option<String>,
    pub sample_tests: Vec<SampleTest>,
}

#[derive(Debug, Clone)]
pub struct SampleTest {
    pub path: String,
    pub body: String,
}

impl PromptContext {
    pub fn empty() -> Self {
        Self {
            source_snippet: String::new(),
            enclosing_symbol: None,
            sample_tests: Vec::new(),
        }
    }
}

pub fn build_suggest_prompt(m: &Mutant, ctx: &PromptContext, model: &str) -> LlmRequest {
    LlmRequest::new(SUGGEST_SYSTEM, render_user(m, ctx)).with_model(model)
}

pub fn build_explain_prompt(m: &Mutant, ctx: &PromptContext, model: &str) -> LlmRequest {
    LlmRequest::new(EXPLAIN_SYSTEM, render_user(m, ctx)).with_model(model)
}

fn render_user(m: &Mutant, ctx: &PromptContext) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Mutation that survived:\n\
         - file: {file}\n\
         - line: {line}\n\
         - operator: {op}\n\
         - original code: `{orig}`\n\
         - replacement code: `{rep}`\n",
        file = m.file.display(),
        line = m.line,
        op = m.operator.name(),
        orig = escape_backticks(&m.original),
        rep = escape_backticks(&m.replacement),
    ));
    if let Some(sym) = &ctx.enclosing_symbol {
        out.push_str(&format!("- enclosing symbol: {sym}\n"));
    }
    if !ctx.source_snippet.is_empty() {
        let snippet = redact(&ctx.source_snippet);
        out.push_str("\nSurrounding source (mutant line marked with ►):\n```python\n");
        out.push_str(&snippet);
        if !snippet.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
    }
    if !ctx.sample_tests.is_empty() {
        out.push_str("\nExisting tests for style reference:\n");
        for s in &ctx.sample_tests {
            out.push_str(&format!(
                "\nFrom `{}`:\n```python\n{}\n```\n",
                s.path,
                redact(&s.body),
            ));
        }
    }
    out.push_str(
        "\nWrite a pytest function that kills this mutation. \
         The function name should describe the behavior being asserted.\n",
    );
    out
}

fn escape_backticks(s: &str) -> String {
    s.replace('`', "\\`")
}

/// Extract the first ```python ... ``` (or bare ``` ... ```) code block
/// from the model's response. Used by `suggest` to materialize the test.
pub fn extract_first_code_block(s: &str) -> Option<String> {
    let mut in_block = false;
    let mut buf = String::new();
    for line in s.lines() {
        let trimmed = line.trim_start();
        if !in_block {
            if trimmed.starts_with("```python") || trimmed == "```" || trimmed.starts_with("```py")
            {
                in_block = true;
                continue;
            }
        } else if trimmed.starts_with("```") {
            return Some(buf);
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if in_block && !buf.is_empty() {
        Some(buf)
    } else {
        None
    }
}

/// Cache key derived from prompt + mutant + file hash. Hex-encoded sha256.
pub fn cache_key(mutant_id: &str, file_sha: &str, prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(mutant_id.as_bytes());
    h.update(b"|");
    h.update(file_sha.as_bytes());
    h.update(b"|");
    h.update(prompt.as_bytes());
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hex sha256 of a file's bytes. Used as part of the cache key so cache
/// entries auto-invalidate when the source changes (same contract as the
/// run-result cache).
pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    fn mutant() -> Mutant {
        Mutant {
            id: "src/calc.py:14:boundary:0".into(),
            file: PathBuf::from("src/calc.py"),
            operator: Operator::BoundaryShift,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "<=".into(),
            replacement: "<".into(),
            line: 14,
            stmt_line: 14,
        }
    }

    #[test]
    fn user_prompt_contains_required_signals() {
        let m = mutant();
        let ctx = PromptContext {
            source_snippet: "  ►   14      return lo <= x and x <= hi\n".into(),
            enclosing_symbol: Some("in_range".into()),
            sample_tests: vec![SampleTest {
                path: "tests/test_calc.py".into(),
                body: "def test_existing():\n    assert True".into(),
            }],
        };
        let req = build_suggest_prompt(&m, &ctx, "claude-test");
        assert_eq!(req.model, "claude-test");
        assert!(req.user.contains("boundary-shift"));
        assert!(req.user.contains("`<=`"));
        assert!(req.user.contains("in_range"));
        assert!(req.user.contains("src/calc.py"));
        assert!(req.user.contains("tests/test_calc.py"));
        assert!(req.system.contains("pytest"));
    }

    #[test]
    fn explain_prompt_uses_distinct_system_message() {
        let m = mutant();
        let ctx = PromptContext::empty();
        let s = build_suggest_prompt(&m, &ctx, "x");
        let e = build_explain_prompt(&m, &ctx, "x");
        assert_ne!(s.system, e.system);
        assert!(e.system.contains("Why it survived"));
    }

    #[test]
    fn extract_first_code_block_finds_python_fence() {
        let raw = "Some prose.\n\n```python\ndef test_x():\n    assert True\n```\n\ntrailing\n";
        let block = extract_first_code_block(raw).unwrap();
        assert!(block.contains("def test_x():"));
        assert!(!block.contains("trailing"));
    }

    #[test]
    fn extract_first_code_block_handles_bare_fence() {
        let raw = "```\ndef t():\n    pass\n```";
        let block = extract_first_code_block(raw).unwrap();
        assert!(block.contains("def t():"));
    }

    #[test]
    fn extract_first_code_block_none_when_absent() {
        assert!(extract_first_code_block("just prose, no fence").is_none());
    }

    #[test]
    fn cache_key_changes_with_any_input() {
        let a = cache_key("m1", "sha-a", "prompt-a");
        let b = cache_key("m1", "sha-a", "prompt-b");
        let c = cache_key("m1", "sha-b", "prompt-a");
        let d = cache_key("m2", "sha-a", "prompt-a");
        for pair in [(&a, &b), (&a, &c), (&a, &d), (&b, &c), (&b, &d), (&c, &d)] {
            assert_ne!(pair.0, pair.1);
        }
        // Stable across calls with same inputs.
        assert_eq!(a, cache_key("m1", "sha-a", "prompt-a"));
    }
}
