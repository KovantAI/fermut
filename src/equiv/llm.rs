//! Layer 5 — LLM equivalence judge.
//!
//! Calls a [`crate::llm::LlmClient`] with the prompt from
//! [`crate::llm::prompt_equiv`] and turns the model's strict-JSON response
//! into an [`EquivVerdict`]. Off by default; opt in via
//! `EquivPipeline::with_llm(...)`.
//!
//! ## Confidence clamp
//!
//! Models systematically over-report confidence on equivalence questions.
//! We accept the verdict only when the model self-reports `>= MIN_CONFIDENCE`
//! and we clamp the reported number into a calibrated band
//! `[CLAMP_LOW, CLAMP_HIGH]` before handing it to the aggregator. The band
//! is wide enough that a high-confidence LLM verdict still needs a
//! corroborating signal (pattern, probe) to clear the pipeline's 0.85
//! threshold — by design.
//!
//! ## Failure handling
//!
//! Any non-success path — transport error, schema mismatch, refusal,
//! counterexample-present-but-equivalent-true — collapses to
//! [`EquivVerdict::NotEquivalent`]. The layer is precision-biased; silence
//! is the safe default.

use std::sync::Arc;

use super::{EquivDetector, EquivVerdict};
use crate::llm::{prompt_equiv::build_equivalence_prompt, LlmClient};
use crate::mutator::Mutant;

/// Floor on the model's self-reported confidence. Below this, treat the
/// response as no-signal regardless of `equivalent` flag.
const MIN_REPORTED_CONFIDENCE: f32 = 0.80;

/// Calibration band that the model's confidence is clamped into before
/// being handed to the pipeline aggregator. Re-fit per model after the
/// first 100 labelled judgements (see eval-corpus section in the design
/// doc).
const CLAMP_LOW: f32 = 0.50;
const CLAMP_HIGH: f32 = 0.80;

/// Default surrounding-source window (lines on each side of the mutation).
const DEFAULT_SURROUND_LINES: usize = 20;

pub struct LlmJudge {
    client: Arc<dyn LlmClient>,
    model: String,
    surround_lines: usize,
}

impl LlmJudge {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self {
            client,
            model: crate::llm::DEFAULT_MODEL.to_string(),
            surround_lines: DEFAULT_SURROUND_LINES,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_surround_lines(mut self, n: usize) -> Self {
        self.surround_lines = n;
        self
    }
}

impl EquivDetector for LlmJudge {
    fn name(&self) -> &'static str {
        "llm-judge"
    }

    fn detect(&self, mutant: &Mutant, source: &str) -> EquivVerdict {
        let snippet = surround(source, mutant.line as usize, self.surround_lines);
        let req = build_equivalence_prompt(mutant, &snippet, None, &self.model);
        let resp = match self.client.complete(&req) {
            Ok(r) => r,
            Err(_) => return EquivVerdict::NotEquivalent,
        };
        let Some(parsed) = parse_verdict(&resp) else {
            return EquivVerdict::NotEquivalent;
        };
        // Contract: a counterexample means equivalent must be false.
        // Treat the contradiction as no-signal rather than trusting either
        // half of an inconsistent response.
        if parsed.equivalent && parsed.counterexample.is_some() {
            return EquivVerdict::NotEquivalent;
        }
        if !parsed.equivalent {
            return EquivVerdict::NotEquivalent;
        }
        if parsed.confidence < MIN_REPORTED_CONFIDENCE {
            return EquivVerdict::NotEquivalent;
        }
        let clamped = clamp(parsed.confidence, CLAMP_LOW, CLAMP_HIGH);
        EquivVerdict::LikelyEquivalent {
            confidence: clamped,
            reason: parsed.reason,
            source: "llm-judge",
        }
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
    let blob = &text[start..=end];
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
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

fn clamp(x: f32, lo: f32, hi: f32) -> f32 {
    if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    }
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
    use crate::mutator::Operator;
    use anyhow::{anyhow, Result};
    use ruff_text_size::{TextRange, TextSize};
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct CannedClient {
        response: Mutex<Result<String, String>>,
        last_user: Mutex<Option<String>>,
    }

    impl CannedClient {
        fn ok(body: &str) -> Self {
            Self {
                response: Mutex::new(Ok(body.to_string())),
                last_user: Mutex::new(None),
            }
        }
        fn err(msg: &str) -> Self {
            Self {
                response: Mutex::new(Err(msg.to_string())),
                last_user: Mutex::new(None),
            }
        }
    }

    impl LlmClient for CannedClient {
        fn complete(&self, req: &LlmRequest) -> Result<String> {
            *self.last_user.lock().unwrap() = Some(req.user.clone());
            match &*self.response.lock().unwrap() {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(anyhow!(e.clone())),
            }
        }
    }

    fn mutant() -> Mutant {
        Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(TextSize::from(0), TextSize::from(1)),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
            site: None,
        }
    }

    #[test]
    fn high_confidence_equivalent_maps_to_clamped_likely() {
        let client = Arc::new(CannedClient::ok(
            r#"{"equivalent": true, "confidence": 0.99, "reason": "identity", "counterexample": null}"#,
        ));
        let judge = LlmJudge::new(client);
        let v = judge.detect(&mutant(), "def f(x): return x + 0\n");
        match v {
            EquivVerdict::LikelyEquivalent {
                confidence, source, ..
            } => {
                assert!((confidence - CLAMP_HIGH).abs() < 1e-6);
                assert_eq!(source, "llm-judge");
            }
            other => panic!("expected LikelyEquivalent, got {other:?}"),
        }
    }

    #[test]
    fn equivalent_false_maps_to_not_equivalent() {
        let client = Arc::new(CannedClient::ok(
            r#"{"equivalent": false, "confidence": 0.95, "reason": "off by one", "counterexample": "1"}"#,
        ));
        let judge = LlmJudge::new(client);
        assert_eq!(
            judge.detect(&mutant(), "def f(x): return x + 1\n"),
            EquivVerdict::NotEquivalent
        );
    }

    #[test]
    fn low_reported_confidence_treated_as_no_signal() {
        let client = Arc::new(CannedClient::ok(
            r#"{"equivalent": true, "confidence": 0.4, "reason": "guess", "counterexample": null}"#,
        ));
        let judge = LlmJudge::new(client);
        assert_eq!(
            judge.detect(&mutant(), "def f(x): return x + 0\n"),
            EquivVerdict::NotEquivalent
        );
    }

    #[test]
    fn contradictory_equivalent_true_with_counterexample_rejected() {
        let client = Arc::new(CannedClient::ok(
            r#"{"equivalent": true, "confidence": 0.99, "reason": "?", "counterexample": "x=1"}"#,
        ));
        let judge = LlmJudge::new(client);
        assert_eq!(
            judge.detect(&mutant(), "def f(x): return x + 0\n"),
            EquivVerdict::NotEquivalent
        );
    }

    #[test]
    fn malformed_json_yields_not_equivalent() {
        let client = Arc::new(CannedClient::ok("not even json, really"));
        let judge = LlmJudge::new(client);
        assert_eq!(
            judge.detect(&mutant(), "def f(): pass\n"),
            EquivVerdict::NotEquivalent
        );
    }

    #[test]
    fn out_of_range_confidence_rejected() {
        let client = Arc::new(CannedClient::ok(
            r#"{"equivalent": true, "confidence": 1.7, "reason": "x", "counterexample": null}"#,
        ));
        let judge = LlmJudge::new(client);
        assert_eq!(
            judge.detect(&mutant(), "def f(): pass\n"),
            EquivVerdict::NotEquivalent
        );
    }

    #[test]
    fn transport_error_yields_not_equivalent() {
        let client = Arc::new(CannedClient::err("boom"));
        let judge = LlmJudge::new(client);
        assert_eq!(
            judge.detect(&mutant(), "def f(): pass\n"),
            EquivVerdict::NotEquivalent
        );
    }

    #[test]
    fn confidence_at_floor_clamped_into_band_low() {
        // Reported 0.80 → above floor; clamped result also 0.80 (band top).
        let client = Arc::new(CannedClient::ok(
            r#"{"equivalent": true, "confidence": 0.80, "reason": "ok", "counterexample": null}"#,
        ));
        let judge = LlmJudge::new(client);
        match judge.detect(&mutant(), "def f(): pass\n") {
            EquivVerdict::LikelyEquivalent { confidence, .. } => {
                assert!((confidence - CLAMP_HIGH).abs() < 1e-6);
            }
            other => panic!("expected LikelyEquivalent, got {other:?}"),
        }
    }

    #[test]
    fn surround_handles_line_past_eof_gracefully() {
        let s = surround("a\nb\nc\n", 99, 2);
        assert!(s.contains("c"));
    }

    #[test]
    fn surround_clips_at_start_of_file() {
        let s = surround("a\nb\nc\nd\ne\n", 1, 2);
        assert_eq!(s, "a\nb\nc");
    }
}
