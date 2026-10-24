//! LLM equivalence judge, used as survivor **triage** in `fermut autofix` and
//! `fermut suggest` (`--triage skip|audit`). Never part of the scoring
//! pipeline: a model's opinion can't prove equivalence, so the mutation score
//! is untouched. What it buys is effort: a survivor the judge calls equivalent
//! isn't worth a test-generation call, and for one it calls different, its
//! explanation (and counterexample) tells the generator what to test. Meta's
//! ACH runs the same LLM-as-judge step before writing tests.
//!
//! Three outcomes, kept apart on purpose:
//!
//! - [`Triage::Equivalent`]: the model says equivalent, with no counterexample
//!   and confidence at least [`MIN_EQUIVALENT_CONFIDENCE`].
//! - [`Triage::Different`]: the model says not equivalent.
//! - [`Triage::Unknown`]: the call failed, the answer didn't parse, it
//!   contradicted itself (equivalent *with* a counterexample), or it said
//!   equivalent without enough confidence. Callers proceed as if untriaged.
//!
//! Prompt: [`crate::llm::prompt_equiv`] (precision-biased, comments stripped,
//! secrets redacted), with the enclosing function as context. Responses go
//! through the same cache as `suggest`, so re-triaging costs nothing.

use std::sync::Mutex;

use serde::Serialize;

use crate::llm::cache::LlmCache;
use crate::llm::prompt::{cache_key, file_sha256, PromptContext};
use crate::llm::prompt_equiv::build_equivalence_prompt;
use crate::llm::{complete_cached, ClientCell, LlmCallOpts, TriageMode};
use crate::mutator::Mutant;

/// Minimum self-reported confidence for an `Equivalent` verdict. High on
/// purpose: a wrong "equivalent" hides a real test gap, a missed one only
/// costs a generation call. ponytail: a constant; make it a flag once
/// `--triage audit` data says where the precision knee is.
const MIN_EQUIVALENT_CONFIDENCE: f32 = 0.9;

/// Context lines on each side of the mutant when it isn't inside a `def`.
const FALLBACK_SURROUND_LINES: usize = 20;

/// Longest enclosing function sent whole; longer ones fall back to the
/// surrounding window so a huge function can't blow up the prompt.
const MAX_FUNCTION_LINES: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Triage {
    Equivalent {
        reason: String,
        confidence: f32,
    },
    Different {
        reason: String,
        counterexample: Option<String>,
    },
    Unknown {
        why: String,
    },
}

impl Triage {
    /// What to tell the test generator, when the judge found a difference.
    pub(crate) fn generation_hint(&self) -> Option<String> {
        let Triage::Different {
            reason,
            counterexample,
        } = self
        else {
            return None;
        };
        let mut hint = reason.trim().to_string();
        if let Some(c) = counterexample {
            hint.push_str(&format!("\nAn input that tells them apart: {c}"));
        }
        (!hint.trim().is_empty()).then_some(hint)
    }

    pub(crate) fn summary(&self, cached: bool) -> TriageSummary {
        let (verdict, reason, counterexample, confidence) = match self {
            Triage::Equivalent { reason, confidence } => {
                ("equivalent", reason.clone(), None, Some(*confidence))
            }
            Triage::Different {
                reason,
                counterexample,
            } => ("different", reason.clone(), counterexample.clone(), None),
            Triage::Unknown { why } => ("unknown", why.clone(), None, None),
        };
        TriageSummary {
            verdict,
            reason,
            counterexample,
            confidence,
            cached,
        }
    }
}

/// The triage verdict as `autofix` / `suggest` report it (JSON contract).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct TriageSummary {
    /// `equivalent`, `different` or `unknown`.
    pub verdict: &'static str,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    pub cached: bool,
}

/// Ask the judge whether `m` is equivalent to the original. `source` is the
/// unmutated file. Returns the verdict and whether it came from the cache.
pub(crate) fn triage(
    m: &Mutant,
    source: &str,
    opts: &LlmCallOpts,
    client_cell: &ClientCell,
    cache: &Mutex<LlmCache>,
) -> (Triage, bool) {
    let context = enclosing_function(source, m.line as usize)
        .unwrap_or_else(|| surround(source, m.line as usize, FALLBACK_SURROUND_LINES));
    let req = build_equivalence_prompt(m, &context, None, &opts.model);
    let sha = file_sha256(&m.file).unwrap_or_default();
    let key = cache_key(&m.id, &sha, &format!("{}\n{}", req.system, req.user));
    let (response, cached) = complete_cached(&req, key, opts, client_cell, cache);
    let verdict = match response {
        Ok(text) => classify(&text),
        Err(e) => Triage::Unknown {
            why: format!("judge call failed: {e}"),
        },
    };
    (verdict, cached)
}

/// The pre-generation step `autofix` and `suggest` share. Under `mode`,
/// triage `m`, put a "different" verdict's explanation into `ctx.hint`, and
/// say whether to skip generation (`skip` mode, judged equivalent). Returns
/// the verdict to report (`None` when triage is off).
pub(crate) fn before_generation(
    mode: TriageMode,
    m: &Mutant,
    ctx: &mut PromptContext,
    opts: &LlmCallOpts,
    client_cell: &ClientCell,
    cache: &Mutex<LlmCache>,
) -> (Option<TriageSummary>, bool) {
    if mode == TriageMode::Off {
        return (None, false);
    }
    let (verdict, cached) = match std::fs::read_to_string(&m.file) {
        Ok(source) => triage(m, &source, opts, client_cell, cache),
        Err(e) => (
            Triage::Unknown {
                why: format!("reading {}: {e}", m.file.display()),
            },
            false,
        ),
    };
    ctx.hint = verdict.generation_hint();
    let skip = mode == TriageMode::Skip && matches!(verdict, Triage::Equivalent { .. });
    (Some(verdict.summary(cached)), skip)
}

/// The inline marker that tells fermut to stop generating this mutant, for a
/// user who agrees it's equivalent (`docs/reference/operators/inline-ignore.md`).
pub(crate) fn ignore_marker(file: impl std::fmt::Display, line: u32, operator: &str) -> String {
    format!("{file}:{line}  # fermut: ignore[{operator}]")
}

fn classify(text: &str) -> Triage {
    let Some(v) = parse_verdict(text) else {
        return Triage::Unknown {
            why: "judge response was not the expected JSON".into(),
        };
    };
    match (v.equivalent, v.counterexample) {
        (true, Some(_)) => Triage::Unknown {
            why: "judge contradicted itself (equivalent, with a counterexample)".into(),
        },
        (true, None) if v.confidence < MIN_EQUIVALENT_CONFIDENCE => Triage::Unknown {
            why: format!(
                "judge leaned equivalent at confidence {:.2} (< {MIN_EQUIVALENT_CONFIDENCE})",
                v.confidence
            ),
        },
        (true, None) => Triage::Equivalent {
            reason: v.reason,
            confidence: v.confidence,
        },
        (false, counterexample) => Triage::Different {
            reason: v.reason,
            counterexample,
        },
    }
}

struct ParsedVerdict {
    equivalent: bool,
    confidence: f32,
    reason: String,
    counterexample: Option<String>,
}

fn parse_verdict(text: &str) -> Option<ParsedVerdict> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let equivalent = v.get("equivalent")?.as_bool()?;
    let confidence = v.get("confidence")?.as_f64()? as f32;
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return None;
    }
    let reason = v
        .get("reason")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let counterexample = v
        .get("counterexample")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(ParsedVerdict {
        equivalent,
        confidence,
        reason,
        counterexample,
    })
}

/// The innermost `def` (or `async def`) block enclosing 1-based `line`,
/// decorators included, as whole source lines. Bounds come from the parsed
/// AST, so multi-line signatures, column-0 comments and dedented string
/// contents don't cut it short. `None` at module or class scope, on a parse
/// failure, or when the function is longer than [`MAX_FUNCTION_LINES`].
fn enclosing_function(source: &str, line: usize) -> Option<String> {
    let line_of = |offset: usize| source[..offset].matches('\n').count() + 1;
    let (start, end) = crate::ast_hash::function_spans(source)
        .into_iter()
        .map(|r| (usize::from(r.start()), usize::from(r.end())))
        .rev()
        .find(|&(s, e)| (line_of(s)..=line_of(e)).contains(&line))?;
    // Widen to whole lines so the def keeps its indentation.
    let start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    let end = source[end..].find('\n').map_or(source.len(), |i| end + i);
    let text = &source[start..end];
    (text.lines().count() <= MAX_FUNCTION_LINES).then(|| text.to_string())
}

fn surround(source: &str, line: usize, span: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let idx = line.saturating_sub(1).min(lines.len() - 1);
    let start = idx.saturating_sub(span);
    let end = (idx + span + 1).min(lines.len());
    lines[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::LlmRequest;
    use crate::llm::LlmClient;
    use crate::mutator::Operator;
    use anyhow::{anyhow, Result};
    use ruff_text_size::{TextRange, TextSize};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CannedClient {
        response: std::result::Result<String, String>,
        calls: AtomicUsize,
        last_user: Mutex<Option<String>>,
    }

    impl CannedClient {
        fn ok(body: &str) -> Arc<Self> {
            Arc::new(Self {
                response: Ok(body.to_string()),
                calls: AtomicUsize::new(0),
                last_user: Mutex::new(None),
            })
        }
        fn err(msg: &str) -> Arc<Self> {
            Arc::new(Self {
                response: Err(msg.to_string()),
                calls: AtomicUsize::new(0),
                last_user: Mutex::new(None),
            })
        }
    }

    impl LlmClient for CannedClient {
        fn complete(&self, req: &LlmRequest) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            *self.last_user.lock().unwrap() = Some(req.user.clone());
            self.response.clone().map_err(|e| anyhow!(e))
        }
    }

    fn mutant(line: u32) -> Mutant {
        Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(TextSize::from(0), TextSize::from(1)),
            original: "+".into(),
            replacement: "-".into(),
            line,
            stmt_line: line,
            site: None,
        }
    }

    fn opts(no_cache: bool) -> LlmCallOpts {
        LlmCallOpts {
            model: "m".into(),
            no_cache,
            cache_path: std::env::temp_dir().join(format!(
                "fermut-triage-{}-{}.json",
                std::process::id(),
                no_cache
            )),
        }
    }

    fn run(client: &Arc<CannedClient>, source: &str, line: u32) -> Triage {
        let cell: ClientCell = std::sync::OnceLock::new();
        let _ = cell.set(Ok(client.clone() as Arc<dyn LlmClient>));
        triage(
            &mutant(line),
            source,
            &opts(true),
            &cell,
            &Mutex::new(LlmCache::default()),
        )
        .0
    }

    const SRC: &str = "def f(x):\n    return x + 0\n";

    #[test]
    fn confident_equivalent_is_equivalent() {
        let c = CannedClient::ok(
            r#"{"equivalent": true, "confidence": 0.95, "reason": "adds zero", "counterexample": null}"#,
        );
        assert_eq!(
            run(&c, SRC, 2),
            Triage::Equivalent {
                reason: "adds zero".into(),
                confidence: 0.95
            }
        );
    }

    #[test]
    fn hesitant_equivalent_is_unknown() {
        let c = CannedClient::ok(
            r#"{"equivalent": true, "confidence": 0.85, "reason": "maybe", "counterexample": null}"#,
        );
        assert!(matches!(run(&c, SRC, 2), Triage::Unknown { .. }));
    }

    #[test]
    fn not_equivalent_is_different_with_a_generation_hint() {
        let c = CannedClient::ok(
            r#"{"equivalent": false, "confidence": 0.9, "reason": "off by one at the boundary", "counterexample": "f(3)"}"#,
        );
        let t = run(&c, SRC, 2);
        assert_eq!(
            t,
            Triage::Different {
                reason: "off by one at the boundary".into(),
                counterexample: Some("f(3)".into())
            }
        );
        let hint = t.generation_hint().unwrap();
        assert!(
            hint.contains("off by one") && hint.contains("f(3)"),
            "{hint}"
        );
    }

    #[test]
    fn contradiction_malformed_and_transport_errors_are_unknown() {
        for c in [
            CannedClient::ok(
                r#"{"equivalent": true, "confidence": 0.99, "reason": "?", "counterexample": "x=1"}"#,
            ),
            CannedClient::ok("no json here"),
            CannedClient::ok(
                r#"{"equivalent": true, "confidence": 1.7, "reason": "x", "counterexample": null}"#,
            ),
            CannedClient::err("boom"),
        ] {
            let t = run(&c, SRC, 2);
            assert!(matches!(t, Triage::Unknown { .. }), "{t:?}");
            assert!(t.generation_hint().is_none());
        }
    }

    #[test]
    fn a_cached_verdict_makes_no_call() {
        let c = CannedClient::ok(
            r#"{"equivalent": false, "confidence": 0.9, "reason": "r", "counterexample": null}"#,
        );
        let cell: ClientCell = std::sync::OnceLock::new();
        let _ = cell.set(Ok(c.clone() as Arc<dyn LlmClient>));
        let cache = Mutex::new(LlmCache::default());
        let o = opts(false);
        let first = triage(&mutant(2), SRC, &o, &cell, &cache);
        let second = triage(&mutant(2), SRC, &o, &cell, &cache);
        assert_eq!((first.1, second.1), (false, true));
        assert_eq!(first.0, second.0);
        assert_eq!(c.calls.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_file(&o.cache_path);
    }

    #[test]
    fn prompt_carries_the_whole_enclosing_function() {
        let c = CannedClient::ok("{}");
        let src = "import os\n\n@cache\ndef outer(a):\n    b = a * 2\n\n    if b > 3:\n        return b + 1\n    return 0\n\nZ = 1\n";
        run(&c, src, 8);
        let user = c.last_user.lock().unwrap().clone().unwrap();
        assert!(user.contains("@cache\ndef outer(a):"), "{user}");
        assert!(user.contains("return 0"), "{user}");
        assert!(
            !user.contains("import os") && !user.contains("Z = 1"),
            "{user}"
        );
    }

    #[test]
    fn enclosing_function_skips_nested_scopes_and_module_level() {
        let src = "class C:\n    def m(self, x):\n        def inner():\n            return 1\n        return x < 2\n";
        let f = enclosing_function(src, 5).unwrap();
        assert!(f.starts_with("    def m(self, x):"), "{f}");
        assert!(enclosing_function("x = 1\ny = 2\n", 2).is_none());
        assert!(enclosing_function("x = 1\n", 2).is_none(), "past EOF");
    }

    #[test]
    fn enclosing_function_length_cap_ignores_where_the_function_starts() {
        let src = format!(
            "{}def f(x):\n    return x\n",
            "x = 1\n".repeat(MAX_FUNCTION_LINES)
        );
        let f = enclosing_function(&src, MAX_FUNCTION_LINES + 2).unwrap();
        assert_eq!(f, "def f(x):\n    return x");
    }

    #[test]
    fn enclosing_function_survives_column_zero_lines_and_wrapped_signatures() {
        let src =
            "def f(x):\n    y = x + 1\n# note\n    s = \"\"\"\nSELECT 1\n\"\"\"\n    return y\n";
        let f = enclosing_function(src, 2).unwrap();
        assert!(f.ends_with("    return y"), "{f}");
        let src = "def f(\n    a,\n) -> int:\n    return a + 1\n";
        let f = enclosing_function(src, 4).unwrap();
        assert_eq!(f, src.trim_end());
    }

    #[test]
    fn unreadable_source_is_unknown_and_never_skips() {
        let c = CannedClient::ok("{}");
        let cell: ClientCell = std::sync::OnceLock::new();
        let _ = cell.set(Ok(c.clone() as Arc<dyn LlmClient>));
        let mut m = mutant(2);
        m.file = PathBuf::from("/nonexistent/fermut/t.py");
        let mut ctx = PromptContext::empty();
        let (summary, skip) = before_generation(
            TriageMode::Skip,
            &m,
            &mut ctx,
            &opts(true),
            &cell,
            &Mutex::new(LlmCache::default()),
        );
        let summary = summary.unwrap();
        assert_eq!(summary.verdict, "unknown");
        assert!(summary.reason.starts_with("reading "), "{}", summary.reason);
        assert!(!skip && ctx.hint.is_none());
        assert!(c.last_user.lock().unwrap().is_none(), "no judge call");
    }

    #[test]
    fn equivalent_at_exactly_the_confidence_floor_counts() {
        let t = classify(
            r#"{"equivalent": true, "confidence": 0.9, "reason": "r", "counterexample": null}"#,
        );
        assert!(matches!(t, Triage::Equivalent { .. }), "{t:?}");
    }

    #[test]
    fn surround_clips_at_the_file_edges() {
        assert_eq!(surround("a\nb\nc\nd\ne\n", 1, 2), "a\nb\nc");
        assert!(surround("a\nb\nc\n", 99, 2).contains('c'));
    }
}
